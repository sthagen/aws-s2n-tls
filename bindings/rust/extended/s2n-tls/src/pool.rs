// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Utilities to handle reusing connections.
//!
//! Creating a single new connection requires significant
//! memory allocations (about 50-60 KB, according to some tests).
//! Instead of allocating memory for a new connection, existing
//! memory can be reused by calling
//! [Connection::wipe()](`crate::connection::Connection::wipe()).
//!
//! On modern systems with reasonably performant allocators, the benefits of reusing
//! connections are reduced. Connection reuse is specifically intended for customers
//! who are sensitive to allocations or for whom allocations are more expensive.
//! Customers are encouraged to run their own benchmarks to determine the exact
//! performance benefit. As a starting point, a simple benchmark comparing allocation
//! against reuse can be found `bench/benches/connection_creation.rs`.
//!
//! The [`Pool`] trait allows applications to define an
//! [Object pool](https://en.wikipedia.org/wiki/Object_pool_pattern) that
//! wipes and stores connections after they are dropped.
//!
//! We also provide a basic Pool implementation, [`ConfigPool`], that
//! implements the pool as a [VecDeque](`std::collections::VecDeque`)
//! with a fixed maximum size.

use crate::{
    config::Config,
    connection::{Builder, Connection},
    enums::Mode,
    error::Error,
};
use std::{
    collections::VecDeque,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

/// A connection produced by a [`Pool`].
///
/// When dropped, returns ownership of the connection to
/// the pool that produced it by calling [`Pool::give`].
#[derive(Debug)]
pub struct PooledConnection<T: Pool = Arc<dyn Pool>> {
    pool: T,
    conn: Option<Connection>,
}

impl<T: Pool> AsRef<Connection> for PooledConnection<T> {
    fn as_ref(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
}

impl<T: Pool> AsMut<Connection> for PooledConnection<T> {
    fn as_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().unwrap()
    }
}

impl<T: Pool> Drop for PooledConnection<T> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.give(conn);
        }
    }
}

impl<T: Pool> Deref for PooledConnection<T> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().unwrap()
    }
}

impl<T: Pool> DerefMut for PooledConnection<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn.as_mut().unwrap()
    }
}

impl<T: Pool + Clone> PooledConnection<T> {
    pub fn new(pool: &T) -> Result<PooledConnection<T>, Error> {
        pool.take().map(|conn| {
            let conn = Some(conn);
            let pool = pool.clone();
            Self { pool, conn }
        })
    }
}

/// An object pool for wiping and reusing connection memory.
///
/// Minimally, an implementation should call [`Connection::wipe()`]
/// during [`Self::give`].
pub trait Pool {
    fn mode(&self) -> Mode;
    fn take(&self) -> Result<Connection, Error>;
    fn give(&self, conn: Connection);
}

impl Pool for Arc<dyn Pool> {
    fn mode(&self) -> Mode {
        self.as_ref().mode()
    }
    fn take(&self) -> Result<Connection, Error> {
        self.as_ref().take()
    }
    fn give(&self, conn: Connection) {
        self.as_ref().give(conn)
    }
}

impl<T: Pool> Pool for Arc<T> {
    fn mode(&self) -> Mode {
        self.as_ref().mode()
    }
    fn take(&self) -> Result<Connection, Error> {
        self.as_ref().take()
    }
    fn give(&self, conn: Connection) {
        self.as_ref().give(conn)
    }
}

/// A pool of Connections. Not a pool of Configs.
///
/// Connections yielded from the pool will always be associated with `config`
/// from [ConfigPoolBuilder::new].
///
/// For discussions about expected performance benefits see [self].
#[derive(Debug)]
pub struct ConfigPool {
    mode: Mode,
    config: Config,
    pool: Mutex<VecDeque<Connection>>,
    max_pool_size: usize,
}

pub type ConfigPoolRef = Arc<ConfigPool>;

/// Builder for [`ConfigPool`].
pub struct ConfigPoolBuilder(ConfigPool);
impl ConfigPoolBuilder {
    pub fn new(mode: Mode, config: Config) -> Self {
        Self(ConfigPool {
            mode,
            config,
            pool: Mutex::new(VecDeque::new()),
            max_pool_size: usize::MAX,
        })
    }

    pub fn set_pool(&mut self, pool: VecDeque<Connection>) -> &mut Self {
        self.0.pool = Mutex::new(pool);
        self
    }

    /// The maximum size of the underlying [`VecDeque`].
    ///
    /// This is NOT the maximum connections that can be created.
    /// When the number of connections created exceeds the `max_pool_size`,
    /// excess reclaimed connections are dropped instead of stored
    /// in the pool.
    ///
    /// If this is not specified, then the max pool size will be usize::MAX
    pub fn set_max_pool_size(&mut self, max_pool_size: usize) -> &mut Self {
        self.0.max_pool_size = max_pool_size;
        self
    }

    pub fn build(self) -> Arc<ConfigPool> {
        Arc::new(self.0)
    }
}

impl ConfigPool {
    pub fn pool_size(&self) -> usize {
        self.pool.lock().map(|pool| pool.len()).unwrap_or(0)
    }

    pub fn is_poisoned(&self) -> bool {
        self.pool.is_poisoned()
    }
}

impl Pool for ConfigPool {
    fn mode(&self) -> Mode {
        self.mode
    }

    /// Get a connection.
    ///
    /// If connections are available in the pool, one will
    /// be returned. Otherwise, a new connection will be created.
    fn take(&self) -> Result<Connection, Error> {
        let from_pool = match self.pool.lock() {
            Ok(mut pool) => pool.pop_front(),
            Err(_) => None,
        };
        let conn = match from_pool {
            // Wiping a connection doesn't wipe the config, but callbacks might
            // have swapped the config so we reset it anyways.
            Some(mut conn) => {
                conn.set_config(self.config.clone())?;
                conn
            }
            // Create a new connection with the stored config.
            None => self.config.build_connection(self.mode)?,
        };
        Ok(conn)
    }

    /// Recycle a connection.
    ///
    /// The connection is wiped and returned to the pool
    /// if space is available.
    fn give(&self, mut conn: Connection) {
        let wiped = conn.wipe().is_ok();
        if let Ok(mut pool) = self.pool.lock() {
            if pool.len() < self.max_pool_size && wiped {
                pool.push_back(conn);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        callbacks::ClientHelloCallback,
        config::{self, Config},
        connection, error,
        testing::TestPair,
    };

    #[test]
    fn config_pool_single_connection() -> Result<(), Box<dyn std::error::Error>> {
        let pool = ConfigPoolBuilder::new(Mode::Server, Config::default()).build();

        // Repeatedly checkout the same connection
        assert_eq!(pool.pool_size(), 0);
        for _ in 0..5 {
            let _conn = PooledConnection::new(&pool)?;
            assert_eq!(pool.pool_size(), 0);
        }
        assert_eq!(pool.pool_size(), 1);
        Ok(())
    }

    #[test]
    fn config_pool_multiple_connections() -> Result<(), Box<dyn std::error::Error>> {
        let pool = ConfigPoolBuilder::new(Mode::Server, Config::default()).build();

        // We need to hold onto connections so that they're not
        // immediately reclaimed by the pool.
        let mut conns = VecDeque::new();

        // Checkout multiple connections
        const COUNT: usize = 25;
        for _ in 0..COUNT {
            conns.push_back(PooledConnection::new(&pool)?);
            assert_eq!(pool.pool_size(), 0);
        }
        assert_eq!(conns.len(), COUNT);

        // Drop all outstanding connections, returning them to the pool
        conns.clear();
        assert_eq!(pool.pool_size(), COUNT);

        // Reuse a subset of the connections
        const SUBSET_COUNT: usize = COUNT / 2;
        for i in 1..=SUBSET_COUNT {
            conns.push_back(PooledConnection::new(&pool)?);
            // The pool should drain as we reuse connections.
            assert_eq!(pool.pool_size(), COUNT - i);
        }
        assert_eq!(conns.len(), SUBSET_COUNT);
        assert_eq!(pool.pool_size(), COUNT - SUBSET_COUNT);

        // Drop all outstanding connections, returning them to the pool
        conns.clear();
        assert_eq!(pool.pool_size(), COUNT);

        Ok(())
    }

    #[test]
    fn config_pool_with_max_size() -> Result<(), Box<dyn std::error::Error>> {
        const POOL_MAX_SIZE: usize = 11;
        let mut pool = ConfigPoolBuilder::new(Mode::Server, Config::default());
        pool.set_max_pool_size(POOL_MAX_SIZE);
        let pool = pool.build();

        // We need to hold onto connections so that they're not
        // immediately reclaimed by the pool.
        let mut conns = VecDeque::new();

        // Create more connections than the pools can hold
        const COUNT: usize = 25;
        for _ in 0..COUNT {
            conns.push_back(PooledConnection::new(&pool)?);
        }
        assert_eq!(conns.len(), COUNT);

        // Drop all outstanding connections, returning them to the pools
        // The pool should now hold its maximum.
        conns.clear();
        assert_eq!(pool.pool_size(), POOL_MAX_SIZE);

        Ok(())
    }

    #[test]
    fn non_generic_pool() -> Result<(), Box<dyn std::error::Error>> {
        let config_pool = ConfigPoolBuilder::new(Mode::Server, Config::default()).build();
        // Note the unwieldy type parameters on PooledConnection here.
        let _: PooledConnection<ConfigPoolRef> = PooledConnection::new(&config_pool)?;
        // To avoid specifying the generic type parameters on PooledConnection,
        // the pool can be converted to an Arc<dyn Pool>.
        let pool: Arc<dyn Pool> = config_pool;
        // Note no generic type parameters on PooledConnection here.
        let _: PooledConnection = PooledConnection::new(&pool)?;
        Ok(())
    }

    #[test]
    fn dereferencing_pooled_connection() -> Result<(), Box<dyn std::error::Error>> {
        let config_pool = ConfigPoolBuilder::new(Mode::Server, Config::default()).build();

        let pooled_conn: PooledConnection<ConfigPoolRef> = PooledConnection::new(&config_pool)?;
        let conn = pooled_conn.deref();
        assert_eq!(pooled_conn.config(), conn.config());

        let mut mut_pooled_conn: PooledConnection<ConfigPoolRef> =
            PooledConnection::new(&config_pool)?;
        let waker = futures_test::task::new_count_waker().0;
        mut_pooled_conn.set_waker(Some(&waker))?;
        assert!(mut_pooled_conn.waker().unwrap().will_wake(&waker));

        Ok(())
    }

    // A yielded connection should always be associated with `config` in the
    // config pool, even if the connection's config is swapped by callbacks.
    #[test]
    fn yielded_connection_associated_config() -> Result<(), error::Error> {
        fn associated_config_has_ch_callback(conn: &connection::Connection) -> bool {
            conn.config()
                .unwrap()
                .context()
                .client_hello_callback
                .is_some()
        }

        struct ConfigSwapCallback(Config);
        impl ClientHelloCallback for ConfigSwapCallback {
            fn on_client_hello(
                &self,
                connection: &mut Connection,
            ) -> crate::callbacks::ConnectionFutureResult {
                connection.set_config(self.0.clone())?;
                Ok(None)
            }
        }

        let empty_config = config::Builder::new().build()?;

        let mut config_with_callback = config::Builder::new();
        let dead_end_callback = ConfigSwapCallback(empty_config);
        config_with_callback.set_client_hello_callback(dead_end_callback)?;
        let config_with_callback = config_with_callback.build()?;

        let config_with_pooled_connections =
            ConfigPoolBuilder::new(Mode::Server, config_with_callback).build();

        let server_from_pool = config_with_pooled_connections.take()?;
        let client = Connection::new_client();
        let mut pair = TestPair::from_connections(client, server_from_pool);

        assert!(associated_config_has_ch_callback(&pair.server));
        assert!(pair.handshake().is_err());
        assert!(!associated_config_has_ch_callback(&pair.server));

        config_with_pooled_connections.give(pair.server);

        let server_from_pool = config_with_pooled_connections.take()?;
        // reused connection once again has callback
        assert!(associated_config_has_ch_callback(&server_from_pool));
        config_with_pooled_connections.give(server_from_pool);

        // assert that there is only a single connection that was getting reused
        assert_eq!(config_with_pooled_connections.pool_size(), 1);
        Ok(())
    }
}
