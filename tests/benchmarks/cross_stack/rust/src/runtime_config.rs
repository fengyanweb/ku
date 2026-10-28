use std::io;

/// Listener policy is independent of executor/CPU topology. Omitting the
/// setting retains the exact historical Tokio/Mio bind path (backlog128 on
/// the supported Linux/macOS/Windows targets); explicit values use TcpSocket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenPlan {
    backlog: u32,
    explicit: bool,
}

impl ListenPlan {
    pub fn parse(value: Option<&str>) -> io::Result<Self> {
        let backlog = match value {
            None => 128,
            Some(value) => value.parse::<u32>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "BENCH_LISTEN_BACKLOG must be 1..65535",
                )
            })?,
        };
        if !(1..=65535).contains(&backlog) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "BENCH_LISTEN_BACKLOG must be 1..65535",
            ));
        }
        Ok(Self {
            backlog,
            explicit: value.is_some(),
        })
    }

    pub fn from_env() -> io::Result<Self> {
        match std::env::var("BENCH_LISTEN_BACKLOG") {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "BENCH_LISTEN_BACKLOG must be valid UTF-8 digits",
            )),
        }
    }

    pub fn requested_backlog(self) -> u32 {
        self.backlog
    }

    pub fn explicit(self) -> bool {
        self.explicit
    }

    pub async fn bind(self, address: &str) -> io::Result<tokio::net::TcpListener> {
        if !self.explicit {
            return tokio::net::TcpListener::bind(address).await;
        }
        // Match TcpListener::bind's address-resolution and ordered fallback
        // behavior, then register the same Tokio async listener with Axum.
        let mut last_error = None;
        for address in tokio::net::lookup_host(address).await? {
            let result = (|| {
                let socket = if address.is_ipv4() {
                    tokio::net::TcpSocket::new_v4()?
                } else {
                    tokio::net::TcpSocket::new_v6()?
                };
                socket.set_reuseaddr(true)?;
                socket.bind(address)?;
                socket.listen(self.backlog)
            })();
            match result {
                Ok(listener) => return Ok(listener),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }
}

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
    fn listener_backlog_defaults_bounds_and_invalid_inputs() {
        let default = ListenPlan::parse(None).unwrap();
        assert_eq!(default.requested_backlog(), 128);
        assert!(!default.explicit());
        for value in ["1", "128", "4096", "65535"] {
            let plan = ListenPlan::parse(Some(value)).unwrap();
            assert_eq!(plan.requested_backlog(), value.parse::<u32>().unwrap());
            assert!(plan.explicit());
        }
        for value in [
            "",
            "0",
            "65536",
            "-1",
            " 128",
            "1.5",
            "invalid",
            "4294967296",
        ] {
            assert_eq!(
                ListenPlan::parse(Some(value)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn listener_default_and_explicit_ephemeral_sockets_accept_and_close() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let runtime = Plan::parse(Some("current-thread"), 1, 1)
            .unwrap()
            .build(None)
            .unwrap();
        runtime.block_on(async {
            for value in [None, Some("1"), Some("4096"), Some("65535")] {
                let listener = ListenPlan::parse(value)
                    .unwrap()
                    .bind("127.0.0.1:0")
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                assert_ne!(address.port(), 0);
                let mut client = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    tokio::net::TcpStream::connect(address),
                )
                .await
                .unwrap()
                .unwrap();
                let (mut server, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                client.write_all(b"x").await.unwrap();
                let mut byte = [0];
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    server.read_exact(&mut byte),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(byte, *b"x");
                drop(server);
                drop(client);
                drop(listener);
            }
        });
    }

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
