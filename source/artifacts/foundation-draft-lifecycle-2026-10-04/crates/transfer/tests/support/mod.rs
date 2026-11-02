use safeflow_contracts::{ExecutionContext, NodeId, OperationId, SessionRef, TaskId, UserId};
use safeflow_runtime::{SessionResolver, StaticSessionResolver};

pub fn context(user: &str, task: &str) -> ExecutionContext {
    let mut sessions = StaticSessionResolver::new();
    let session = SessionRef::parse("test_server_session").unwrap();
    sessions.insert(session.clone(), UserId::parse(user).unwrap());
    ExecutionContext {
        authenticated_user_id: sessions.resolve(&session).unwrap(),
        task_id: TaskId::parse(task).unwrap(),
        node_id: NodeId::parse("query_node").unwrap(),
        operation_id: OperationId::parse("query_operation_not_transfer").unwrap(),
        plan_version: 1,
        idempotency_key: None,
    }
}

pub use safeflow_transfer::fixtures::ManualClock;

// The same dependency-free executor as the foundation's read-only example.
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}
