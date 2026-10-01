//! A tokio runtime whose threads have the stack Lisp code needs.

/// The native stack of each thread the runtime starts, its workers
/// and its blocking threads, where Lisp code runs. tulisp's default
/// limits in a release build, 1000 nested calls and 4000 levels of
/// nesting in a form, need about 3 KB of stack a call and about 650
/// bytes a level; tokio's default of 2 MiB fits about 730 calls, so
/// code under the limit could overflow the stack and abort the
/// process. 8 MiB, a main thread's usual stack, is what tulisp's
/// limits are set for.
const THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// A multi-thread tokio runtime, with all its drivers, whose threads
/// have the stack Lisp code needs.
pub fn build() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK_SIZE)
        .build()
}

#[cfg(test)]
mod tests {
    use tulisp::TulispContext;

    /// Recursion nearly as deep as tulisp's default limit allows:
    /// 1000 calls, or 64 in a debug build, whose stack frames are
    /// larger.
    const DEEP_RECURSION: &str = if cfg!(debug_assertions) {
        "(defun count-down (n) (if (= n 0) 0 (+ 1 (count-down (1- n))))) (count-down 60)"
    } else {
        "(defun count-down (n) (if (= n 0) 0 (+ 1 (count-down (1- n))))) (count-down 990)"
    };

    /// What `DEEP_RECURSION` returns.
    const DEPTH: &str = if cfg!(debug_assertions) { "60" } else { "990" };

    #[test]
    fn lisp_on_a_blocking_thread_recurses_to_tulisps_limit() {
        let runtime = super::build().expect("runtime builds");
        let result = runtime.block_on(async {
            tokio::task::spawn_blocking(|| {
                TulispContext::new()
                    .eval_string(DEEP_RECURSION)
                    .map(|value| value.to_string())
                    .map_err(|e| e.to_string())
            })
            .await
        });
        assert_eq!(result.expect("the task ends"), Ok(DEPTH.to_string()));
    }

    #[test]
    fn lisp_on_a_worker_thread_recurses_to_tulisps_limit() {
        let runtime = super::build().expect("runtime builds");
        let result = runtime.block_on(async {
            tokio::spawn(async {
                TulispContext::new()
                    .eval_string(DEEP_RECURSION)
                    .map(|value| value.to_string())
                    .map_err(|e| e.to_string())
            })
            .await
        });
        assert_eq!(result.expect("the task ends"), Ok(DEPTH.to_string()));
    }
}
