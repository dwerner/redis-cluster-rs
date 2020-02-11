use std::{
    cell::Cell,
    error::Error,
    sync::{Mutex, MutexGuard},
};

use {
    futures::{prelude::*, stream},
    proptest::proptest,
    tokio::runtime::Runtime,
};

use redis_cluster_async::{
    redis::{cmd, RedisError, RedisResult},
    Client,
};

const REDIS_URL: &str = "redis://127.0.0.1:7000/";

pub struct RedisProcess;
pub struct RedisLock(MutexGuard<'static, RedisProcess>);

impl RedisProcess {
    // Blocks until we have sole access.
    pub fn lock() -> RedisLock {
        lazy_static::lazy_static! {
            static ref REDIS: Mutex<RedisProcess> = Mutex::new(RedisProcess {});
        }

        // If we panic in a test we don't want subsequent to fail because of a poisoned error
        let redis_lock = REDIS
            .lock()
            .unwrap_or_else(|poison_error| poison_error.into_inner());
        RedisLock(redis_lock)
    }
}

// ----------------------------------------------------------------------------

pub struct RedisEnv {
    _redis_lock: RedisLock,
    pub runtime: Runtime,
    pub client: Client,
    nodes: Vec<redis::aio::MultiplexedConnection>,
}

impl RedisEnv {
    pub fn new() -> Self {
        let _ = env_logger::try_init();

        let mut runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let redis_lock = RedisProcess::lock();

        let redis_client = redis::Client::open(REDIS_URL)
            .unwrap_or_else(|_| panic!("Failed to connect to '{}'", REDIS_URL));

        let (client, nodes) = runtime.block_on(async {
            let node_infos = loop {
                let node_infos = async {
                    let mut conn = redis_client.get_multiplexed_tokio_connection().await?;
                    Self::cluster_info(&mut conn).await
                }
                .await
                .expect("Unable to query nodes for information");
                // Wait for the cluster to stabilize
                if node_infos.iter().filter(|(_, master)| *master).count() == 3 {
                    break node_infos;
                }
                tokio::time::delay_for(std::time::Duration::from_millis(100)).await;
            };
            let mut node_urls = Vec::new();
            let mut nodes = Vec::new();
            // Clear databases:
            for (url, master) in node_infos {
                let redis_client = redis::Client::open(&url[..])
                    .unwrap_or_else(|_| panic!("Failed to connect to '{}'", url));
                let mut conn = redis_client
                    .get_multiplexed_tokio_connection()
                    .await
                    .unwrap();

                if master {
                    node_urls.push(url.to_string());
                    let () = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        redis::Cmd::new().arg("FLUSHALL").query_async(&mut conn),
                    )
                    .await
                    .unwrap_or_else(|err| panic!("Unable to flush {}: {}", url, err))
                    .unwrap_or_else(|err| panic!("Unable to flush {}: {}", url, err));
                }

                nodes.push(conn);
            }

            let client = Client::open(node_urls.iter().map(|s| &s[..]).collect()).unwrap();
            (client, nodes)
        });

        RedisEnv {
            runtime,
            client,
            nodes,
            _redis_lock: redis_lock,
        }
    }

    async fn cluster_info<T>(redis_client: &mut T) -> RedisResult<Vec<(String, bool)>>
    where
        T: Clone + redis::aio::ConnectionLike + Send + 'static,
    {
        redis::cmd("CLUSTER")
            .arg("NODES")
            .query_async(redis_client)
            .map_ok(|s: String| {
                s.lines()
                    .map(|line| {
                        let mut iter = line.split(' ');
                        let port = iter
                            .by_ref()
                            .nth(1)
                            .expect("Node ip")
                            .splitn(2, '@')
                            .next()
                            .unwrap()
                            .splitn(2, ':')
                            .nth(1)
                            .unwrap();
                        (
                            format!("redis://localhost:{}", port),
                            iter.next().expect("master").contains("master"),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .await
    }
}

#[test]
fn basic_cmd() {
    let mut env = RedisEnv::new();
    let client = env.client;
    env.runtime
        .block_on(async {
            let mut connection = client.get_connection().await?;
            let () = cmd("SET")
                .arg("test")
                .arg("test_data")
                .query_async(&mut connection)
                .await?;
            let res: String = cmd("GET")
                .arg("test")
                .clone()
                .query_async(&mut connection)
                .await?;
            assert_eq!(res, "test_data");
            Ok(())
        })
        .map_err(|err: RedisError| err)
        .unwrap()
}

#[ignore] // TODO Handle pipe where the keys do not all go to the same node
#[test]
fn basic_pipe() {
    let mut env = RedisEnv::new();
    let client = env.client;
    env.runtime
        .block_on(async {
            let mut connection = client.get_connection().await?;
            let mut pipe = redis::pipe();
            pipe.add_command(cmd("SET").arg("test").arg("test_data").clone());
            pipe.add_command(cmd("SET").arg("test3").arg("test_data3").clone());
            let () = pipe.query_async(&mut connection).await?;
            let res: String = cmd("GET").arg("test").query_async(&mut connection).await?;
            assert_eq!(res, "test_data");
            let res: String = cmd("GET")
                .arg("test3")
                .clone()
                .query_async(&mut connection)
                .await?;
            assert_eq!(res, "test_data3");
            Ok(())
        })
        .map_err(|err: RedisError| err)
        .unwrap()
}

#[test]
fn xtrim_cmd() {
    let mut env = RedisEnv::new();
    let client = env.client;
    env.runtime
        .block_on(async {
            let mut connection = client.get_connection().await?;
            redis::cmd("XTRIM")
                .arg("mystream")
                .arg("MAXLEN")
                .arg("~")
                .query_async(&mut connection)
                .await?;
            Ok(())
        })
        .map_err(|err: RedisError| err)
        .unwrap()
}

#[test]
fn proptests() {
    let env = std::cell::RefCell::new(FailoverEnv::new());

    proptest!(
        proptest::prelude::ProptestConfig { cases: 30, failure_persistence: None, .. Default::default() },
        |(requests in 0..15, value in 0..i32::max_value())| {
            test_failover(&mut env.borrow_mut(), requests, value)
        }
    );
}

#[test]
fn basic_failover() {
    test_failover(&mut FailoverEnv::new(), 10, 123);
}

struct FailoverEnv {
    env: RedisEnv,
    connection: redis_cluster_async::Connection,
}

impl FailoverEnv {
    fn new() -> Self {
        let mut env = RedisEnv::new();
        let connection = env.runtime.block_on(env.client.get_connection()).unwrap();

        FailoverEnv { env, connection }
    }
}

async fn do_failover(
    redis: &mut redis::aio::MultiplexedConnection,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    cmd("CLUSTER")
        .arg("FAILOVER")
        .query_async(redis)
        .err_into()
        .await
}

fn test_failover(env: &mut FailoverEnv, requests: i32, value: i32) {
    let completed = Cell::new(0);
    let completed = &completed;

    let FailoverEnv { env, connection } = env;

    let nodes = env.nodes.clone();

    let test_future = async {
        (0..requests + 1)
            .map(|i| {
                let mut connection = connection.clone();
                let mut nodes = nodes.clone();
                async move {
                    if i == requests / 2 {
                        // Failover all the nodes, error only if all the failover requests error
                        nodes.iter_mut().map(|node| do_failover(node))
                            .collect::<stream::FuturesUnordered<_>>()
                            .fold(
                                Err(Box::<dyn Error + Send + Sync>::from("None".to_string())),
                                |acc: Result<(), Box<dyn Error + Send + Sync>>,
                                 result: Result<(), Box<dyn Error + Send + Sync>>| async move {
                                    acc.or_else(|_| result)
                                },
                            )
                            .await
                    } else {
                        let key = format!("test-{}-{}", value, i);
                        let () = cmd("SET")
                            .arg(&key)
                            .arg(i)
                            .clone()
                            .query_async(&mut connection)
                            .await?;
                        let res: i32 = cmd("GET")
                            .arg(key)
                            .clone()
                            .query_async(&mut connection)
                            .await?;
                        assert_eq!(res, i);
                        completed.set(completed.get() + 1);
                        Ok(())
                    }
                }
            })
            .collect::<stream::FuturesUnordered<_>>()
            .try_collect()
            .await
    };
    env.runtime
        .block_on(test_future)
        .unwrap_or_else(|err| panic!("{}", err));
    assert_eq!(completed.get(), requests, "Some requests never completed!");
}
