use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError,
};

const GAS_FOR_CALLBACK: Gas = Gas::from_tgas(10);

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(crate = "near_sdk::serde")]
#[borsh(crate = "near_sdk::borsh")]
pub enum TaskState {
    Created,
    Submitted,
    TransferPending { to_worker: bool },
    Released,
    Failed,
    TimedOut,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
#[borsh(crate = "near_sdk::borsh")]
pub struct Task {
    pub task_id: String,
    pub buyer: AccountId,
    pub worker: AccountId,
    pub amount: U128,
    pub criteria_hash: String,
    pub authorized_verifier: AccountId,
    pub deadline_ns: u64,
    pub state: TaskState,
    pub submission_hash: Option<String>,
    pub verification_passed: Option<bool>,
    pub failed_test_ids: Option<Vec<String>>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    tasks: LookupMap<String, Task>,
}

#[near]
impl Contract {
    #[init]
    pub fn new() -> Self {
        Self {
            tasks: LookupMap::new(b"t"),
        }
    }

    #[payable]
    pub fn create_task(
        &mut self,
        task_id: String,
        worker: AccountId,
        criteria_hash: String,
        authorized_verifier: AccountId,
        deadline_ns: u64,
    ) {
        let amount = env::attached_deposit();
        assert!(amount > NearToken::from_yoctonear(0), "Deposit must be > 0");
        assert!(!task_id.is_empty(), "task_id must be non-empty");
        assert!(!self.tasks.contains_key(&task_id), "Task ID already exists");
        assert!(deadline_ns > env::block_timestamp(), "Deadline must be in the future");
        assert!(is_valid_sha256_hex(&criteria_hash), "criteria_hash must be 64-char lowercase hex");
        assert_ne!(worker, env::predecessor_account_id(), "Worker cannot be the buyer");

        let task = Task {
            task_id: task_id.clone(),
            buyer: env::predecessor_account_id(),
            worker,
            amount: U128(amount.as_yoctonear()),
            criteria_hash,
            authorized_verifier,
            deadline_ns,
            state: TaskState::Created,
            submission_hash: None,
            verification_passed: None,
            failed_test_ids: None,
        };
        self.tasks.insert(task_id, task);
    }

    pub fn submit(&mut self, task_id: String, submission_hash: String) {
        let task = self.tasks.get_mut(&task_id).expect("Task not found");
        assert_eq!(task.state, TaskState::Created, "Task must be in Created state");
        assert_eq!(env::predecessor_account_id(), task.worker, "Only the designated worker may submit");
        assert!(env::block_timestamp() < task.deadline_ns, "Deadline has passed");
        assert!(is_valid_sha256_hex(&submission_hash), "submission_hash must be 64-char lowercase hex");

        task.state = TaskState::Submitted;
        task.submission_hash = Some(submission_hash);
    }

    pub fn attest(
        &mut self,
        task_id: String,
        submission_hash: String,
        passed: bool,
        failed_test_ids: Option<Vec<String>>,
    ) {
        let task = self.tasks.get_mut(&task_id).expect("Task not found");
        assert_eq!(task.state, TaskState::Submitted, "Task must be in Submitted state");
        assert_eq!(env::predecessor_account_id(), task.authorized_verifier, "Only the authorized verifier may attest");
        assert!(env::block_timestamp() < task.deadline_ns, "Cannot attest at or after deadline");
        let expected = task.submission_hash.as_ref().expect("No submission hash recorded");
        assert_eq!(&submission_hash, expected, "Submission hash mismatch");

        task.verification_passed = Some(passed);
        task.failed_test_ids = failed_test_ids;

        let amount = NearToken::from_yoctonear(task.amount.0);
        let recipient = if passed { task.worker.clone() } else { task.buyer.clone() };
        task.state = TaskState::TransferPending { to_worker: passed };


        Promise::new(recipient)
            .transfer(amount)
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_CALLBACK)
                    .on_transfer_complete(task_id, passed),
            );
    }

    pub fn timeout_refund(&mut self, task_id: String) {
        let task = self.tasks.get_mut(&task_id).expect("Task not found");
        assert!(
            matches!(task.state, TaskState::Created | TaskState::Submitted),
            "Timeout only allowed from Created or Submitted"
        );
        assert!(env::block_timestamp() >= task.deadline_ns, "Deadline has not been reached");

        let amount = NearToken::from_yoctonear(task.amount.0);
        let buyer = task.buyer.clone();
        task.state = TaskState::TransferPending { to_worker: false };

        Promise::new(buyer)
            .transfer(amount)
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_CALLBACK)
                    .on_timeout_transfer_complete(task_id),
            );
    }

    #[private]
    pub fn on_transfer_complete(
        &mut self,
        task_id: String,
        passed: bool,
        #[callback_result] result: Result<(), PromiseError>,
    ) {
        let task = match self.tasks.get_mut(&task_id) {
            Some(t) => t,
            None => return,
        };
        if !matches!(task.state, TaskState::TransferPending { .. }) {
            return;
        }
        match result {
            Ok(()) => {
                task.state = if passed { TaskState::Released } else { TaskState::Failed };
            }
            Err(_) => {
                env::log_str(&format!("Transfer failed for task {}; remaining in TransferPending", task_id));
            }
        }
    }

    #[private]
    pub fn on_timeout_transfer_complete(
        &mut self,
        task_id: String,
        #[callback_result] result: Result<(), PromiseError>,
    ) {
        let task = match self.tasks.get_mut(&task_id) {
            Some(t) => t,
            None => return,
        };
        if !matches!(task.state, TaskState::TransferPending { .. }) {
            return;
        }
        match result {
            Ok(()) => {
                task.state = TaskState::TimedOut;
            }
            Err(_) => {
                env::log_str(&format!("Timeout transfer failed for task {}; remaining in TransferPending", task_id));
            }
        }
    }

    pub fn retry_transfer(&mut self, task_id: String) {
        let task = self.tasks.get(&task_id).expect("Task not found");
        let to_worker = match &task.state {
            TaskState::TransferPending { to_worker } => *to_worker,
            _ => env::panic_str("Task is not in TransferPending state"),
        };
        let amount = NearToken::from_yoctonear(task.amount.0);
        let recipient = if to_worker { task.worker.clone() } else { task.buyer.clone() };
        let is_timeout = task.verification_passed.is_none();

        if to_worker {
            Promise::new(recipient)
                .transfer(amount)
                .then(
                    Self::ext(env::current_account_id())
                        .with_static_gas(GAS_FOR_CALLBACK)
                        .on_transfer_complete(task_id, true),
                );
        } else if is_timeout {
            Promise::new(recipient)
                .transfer(amount)
                .then(
                    Self::ext(env::current_account_id())
                        .with_static_gas(GAS_FOR_CALLBACK)
                        .on_timeout_transfer_complete(task_id),
                );
        } else {
            Promise::new(recipient)
                .transfer(amount)
                .then(
                    Self::ext(env::current_account_id())
                        .with_static_gas(GAS_FOR_CALLBACK)
                        .on_transfer_complete(task_id, false),
                );
        }
    }

    pub fn get_task(&self, task_id: String) -> Option<Task> {
        self.tasks.get(&task_id).cloned()
    }
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, NearToken};

    fn setup_context(predecessor: AccountId, deposit: NearToken, timestamp_ns: u64) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .predecessor_account_id(predecessor)
            .attached_deposit(deposit)
            .block_timestamp(timestamp_ns);
        builder
    }

    fn valid_hash() -> String { "a".repeat(64) }
    fn valid_hash2() -> String { "b".repeat(64) }

    #[test]
    fn create_task_success() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 2_000);
        let t = contract.get_task("task1".into()).unwrap();
        assert_eq!(t.buyer, accounts(1));
        assert_eq!(t.worker, accounts(2));
        assert_eq!(t.amount.0, NearToken::from_near(1).as_yoctonear());
        assert_eq!(t.state, TaskState::Created);
    }

    #[test]
    #[should_panic(expected = "Deposit must be > 0")]
    fn create_zero_deposit() {
        let ctx = setup_context(accounts(1), NearToken::from_yoctonear(0), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 2_000);
    }

    #[test]
    #[should_panic(expected = "Task ID already exists")]
    fn create_duplicate_id() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 2_000);
        let ctx2 = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx2.build());
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 2_000);
    }

    #[test]
    #[should_panic(expected = "Deadline must be in the future")]
    fn create_invalid_deadline() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 2_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 2_000);
    }

    #[test]
    #[should_panic(expected = "criteria_hash must be 64-char lowercase hex")]
    fn create_malformed_hash() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), "nothex".into(), accounts(3), 2_000);
    }

    #[test]
    fn submit_success() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let t = contract.get_task("task1".into()).unwrap();
        assert_eq!(t.state, TaskState::Submitted);
        assert_eq!(t.submission_hash, Some(valid_hash2()));
    }

    #[test]
    #[should_panic(expected = "Only the designated worker may submit")]
    fn submit_wrong_worker() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(4), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
    }

    #[test]
    #[should_panic(expected = "Task must be in Created state")]
    fn double_submission() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(2), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.submit("task1".into(), valid_hash2());
    }

    #[test]
    #[should_panic(expected = "Deadline has passed")]
    fn submit_after_deadline() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 5_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 5_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
    }

    #[test]
    fn attest_pass_moves_to_transfer_pending() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
        let t = contract.get_task("task1".into()).unwrap();
        assert!(matches!(t.state, TaskState::TransferPending { to_worker: true }));
        assert_eq!(t.verification_passed, Some(true));
    }

    #[test]
    fn attest_fail_moves_to_transfer_pending() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), false, Some(vec!["test1".into()]));
        let t = contract.get_task("task1".into()).unwrap();
        assert!(matches!(t.state, TaskState::TransferPending { to_worker: false }));
        assert_eq!(t.verification_passed, Some(false));
    }

    #[test]
    #[should_panic(expected = "Only the authorized verifier may attest")]
    fn unauthorized_verifier() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(4), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
    }

    #[test]
    #[should_panic(expected = "Cannot attest at or after deadline")]
    fn attestation_after_deadline() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 5_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 5_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
    }

    #[test]
    #[should_panic(expected = "Task must be in Submitted state")]
    fn attestation_from_wrong_state() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
    }

    #[test]
    #[should_panic(expected = "Submission hash mismatch")]
    fn submission_hash_mismatch() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash(), true, None);
    }

    #[test]
    fn timeout_from_created() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 5_000);
        let ctx2 = setup_context(accounts(4), NearToken::from_yoctonear(0), 5_000);
        testing_env!(ctx2.build());
        contract.timeout_refund("task1".into());
        let t = contract.get_task("task1".into()).unwrap();
        assert!(matches!(t.state, TaskState::TransferPending { to_worker: false }));
    }

    #[test]
    fn timeout_from_submitted() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 5_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(4), NearToken::from_yoctonear(0), 5_000);
        testing_env!(ctx3.build());
        contract.timeout_refund("task1".into());
        let t = contract.get_task("task1".into()).unwrap();
        assert!(matches!(t.state, TaskState::TransferPending { to_worker: false }));
    }

    #[test]
    #[should_panic(expected = "Deadline has not been reached")]
    fn timeout_before_deadline() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 5_000);
        let ctx2 = setup_context(accounts(4), NearToken::from_yoctonear(0), 4_999);
        testing_env!(ctx2.build());
        contract.timeout_refund("task1".into());
    }

    #[test]
    fn exact_deadline_boundary_timeout_ok() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 5_000);
        let ctx2 = setup_context(accounts(4), NearToken::from_yoctonear(0), 5_000);
        testing_env!(ctx2.build());
        contract.timeout_refund("task1".into());
        assert!(contract.get_task("task1".into()).is_some());
    }

    #[test]
    fn callback_success_sets_released() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
        let ctx4 = setup_context(accounts(0), NearToken::from_yoctonear(0), 4_000);
        testing_env!(ctx4.build());
        contract.on_transfer_complete("task1".into(), true, Ok(()));
        let t = contract.get_task("task1".into()).unwrap();
        assert_eq!(t.state, TaskState::Released);
    }

    #[test]
    fn callback_success_sets_failed() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), false, None);
        let ctx4 = setup_context(accounts(0), NearToken::from_yoctonear(0), 4_000);
        testing_env!(ctx4.build());
        contract.on_transfer_complete("task1".into(), false, Ok(()));
        let t = contract.get_task("task1".into()).unwrap();
        assert_eq!(t.state, TaskState::Failed);
    }

    #[test]
    fn callback_failure_leaves_pending() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
        let t = contract.get_task("task1".into()).unwrap();
        assert!(matches!(t.state, TaskState::TransferPending { .. }));
    }

    #[test]
    fn retry_transfer_only_from_pending() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(4), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.retry_transfer("task1".into());
        }));
        assert!(result.is_err());
    }

    #[test]
    fn no_unintended_withdrawal_after_release() {
        let ctx = setup_context(accounts(1), NearToken::from_near(1), 1_000);
        testing_env!(ctx.build());
        let mut contract = Contract::new();
        contract.create_task("task1".into(), accounts(2), valid_hash(), accounts(3), 10_000);
        let ctx2 = setup_context(accounts(2), NearToken::from_yoctonear(0), 2_000);
        testing_env!(ctx2.build());
        contract.submit("task1".into(), valid_hash2());
        let ctx3 = setup_context(accounts(3), NearToken::from_yoctonear(0), 3_000);
        testing_env!(ctx3.build());
        contract.attest("task1".into(), valid_hash2(), true, None);
        let ctx4 = setup_context(accounts(0), NearToken::from_yoctonear(0), 4_000);
        testing_env!(ctx4.build());
        contract.on_transfer_complete("task1".into(), true, Ok(()));
        contract.on_transfer_complete("task1".into(), true, Ok(()));
        let t = contract.get_task("task1".into()).unwrap();
        assert_eq!(t.state, TaskState::Released);
        let ctx5 = setup_context(accounts(4), NearToken::from_yoctonear(0), 20_000);
        testing_env!(ctx5.build());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.timeout_refund("task1".into());
        }));
        assert!(result.is_err());
    }
}
