use std::io;

pub(super) async fn connected(result: io::Result<bool>) -> bool {
    match result {
        Ok(connected) => connected,
        Err(err) => {
            eprintln!(
                "{} herdvia: warning: pipe state query failed; disconnect monitoring disabled: {err}",
                crate::log::stamp()
            );
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    #[test]
    fn successful_queries_preserve_the_connection_state() {
        let mut context = Context::from_waker(Waker::noop());
        for state in [true, false] {
            assert_eq!(
                pin!(connected(Ok(state))).poll(&mut context),
                Poll::Ready(state)
            );
        }
    }

    #[test]
    fn failed_query_stays_pending_instead_of_reporting_disconnection() {
        let mut context = Context::from_waker(Waker::noop());
        let mut watcher = pin!(connected(Err(io::Error::other("state query failed"))));
        assert!(watcher.as_mut().poll(&mut context).is_pending());
        assert!(watcher.as_mut().poll(&mut context).is_pending());
    }
}
