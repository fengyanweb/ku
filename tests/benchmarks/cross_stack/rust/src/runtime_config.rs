use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topology {
    MultiThread,
    CurrentThread,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    topology: Topology,
    workers: usize,
    pub cpu_workers: usize,
}

impl Plan {
    pub fn parse(name: Option<&str>, workers: usize, cpu_workers: usize) -> io::Result<Self> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
        let topology = match name {
            None | Some("multi-thread") => Topology::MultiThread,
            Some("current-thread") => Topology::CurrentThread,
            _ => {
                return Err(invalid(
                    "BENCH_RUNTIME must be multi-thread or current-thread",
                ))
            }
        };
        if !(1..=4).contains(&workers) || !(1..=4).contains(&cpu_workers) {
            return Err(invalid("runtime worker counts must be 1..4"));
        }
        if topology == Topology::CurrentThread && workers != 1 {
            return Err(invalid("current-thread requires explicit BENCH_WORKERS=1"));
        }
        Ok(Self {
            topology,
            workers,
            cpu_workers,
        })
    }

    pub fn name(self) -> &'static str {
        match self.topology {
            Topology::MultiThread => "multi-thread",
            Topology::CurrentThread => "current-thread",
        }
    }

    pub fn async_worker_threads(self) -> usize {
        match self.topology {
            Topology::MultiThread => self.workers,
            Topology::CurrentThread => 0,
        }
    }

    pub fn main_thread_role(self) -> &'static str {
        match self.topology {
            Topology::MultiThread => "block_on_acceptor",
            Topology::CurrentThread => "io_runtime",
        }
    }

    pub fn build(self, event_interval: Option<u32>) -> io::Result<tokio::runtime::Runtime> {
        let mut builder = match self.topology {
            Topology::MultiThread => {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(self.workers);
                builder
            }
            Topology::CurrentThread => tokio::runtime::Builder::new_current_thread(),
        };
        builder.max_blocking_threads(self.cpu_workers).enable_all();
        if let Some(interval) = event_interval {
            builder.event_interval(interval);
        }
        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keeps_existing_multi_thread_reference() {
        let plan = Plan::parse(None, 2, 2).unwrap();
        assert_eq!(plan.name(), "multi-thread");
        assert_eq!(plan.async_worker_threads(), 2);
        assert_eq!(plan.cpu_workers, 2);
        assert_eq!(plan.main_thread_role(), "block_on_acceptor");
    }

    #[test]
    fn current_thread_does_not_claim_an_async_worker() {
        let plan = Plan::parse(Some("current-thread"), 1, 1).unwrap();
        assert_eq!(plan.async_worker_threads(), 0);
        assert_eq!(plan.main_thread_role(), "io_runtime");
        assert!(Plan::parse(Some("current-thread"), 2, 1).is_err());
        assert!(Plan::parse(Some("other"), 1, 1).is_err());
        assert!(Plan::parse(None, 0, 1).is_err());
    }

    #[test]
    fn current_thread_io_can_await_the_bounded_cpu_worker() {
        let runtime = Plan::parse(Some("current-thread"), 1, 1)
            .unwrap()
            .build(Some(1))
            .unwrap();
        assert_eq!(
            runtime.handle().runtime_flavor(),
            tokio::runtime::RuntimeFlavor::CurrentThread
        );
        runtime.block_on(async {
            let caller = std::thread::current().id();
            let worker = tokio::task::spawn_blocking(|| (std::thread::current().id(), 42))
                .await
                .unwrap();
            assert_ne!(caller, worker.0);
            assert_eq!(worker.1, 42);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        });
    }
}
