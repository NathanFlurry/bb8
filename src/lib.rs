//! A generic connection pool, designed for asynchronous tokio-based connections
//! This is an asynchronous tokio-based version of r2d2.
//!
//! Opening a new database connection every time one is needed is both
//! inefficient and can lead to resource exhaustion under high traffic
//! conditions. A connection pool maintains a set of open connections to a
//! database, handing them out for repeated use.
//!
//! bb8 is agnostic to the connection type it is managing. Implementors of the
//! `ManageConnection` trait provide the database-specific logic to create and
//! check the health of connections.
#![deny(missing_docs, missing_debug_implementations)]

extern crate futures;
extern crate tokio_executor;
extern crate tokio_timer;

use std::borrow::BorrowMut;
use std::cmp::{max, min};
use std::collections::VecDeque;
use std::error;
use std::fmt;
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use futures::future::{lazy, loop_fn, ok, Either, Loop};
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use futures::sync::oneshot;
use tokio_executor::spawn;
use tokio_timer::{Interval, Timeout};

mod util;
use util::*;

/// A trait which provides connection-specific functionality.
pub trait ManageConnection: Send + Sync + 'static {
    /// The connection type this manager deals with.
    type Connection: Send + 'static;
    /// The error type returned by `Connection`s.
    type Error: Send + 'static;

    /// Attempts to create a new connection.
    fn connect(&self) -> Box<dyn Future<Item = Self::Connection, Error = Self::Error> + Send>;
    /// Determines if the connection is still connected to the database.
    fn is_valid(
        &self,
        conn: Self::Connection,
    ) -> Box<dyn Future<Item = Self::Connection, Error = (Self::Error, Self::Connection)> + Send>;
    /// Synchronously determine if the connection is no longer usable, if possible.
    fn has_broken(&self, conn: &mut Self::Connection) -> bool;
}

/// bb8's error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError<E> {
    /// An error returned from user code.
    User(E),
    /// bb8 attempted to get a connection but the provided timeout was exceeded.
    TimedOut,
}

impl<E> fmt::Display for RunError<E>
where
    E: error::Error + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            RunError::User(ref err) => write!(f, "{}", err),
            RunError::TimedOut => write!(f, "Timed out in bb8"),
        }
    }
}

impl<E> error::Error for RunError<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            RunError::User(ref err) => Some(err),
            RunError::TimedOut => None,
        }
    }
}

/// A trait to receive errors generated by connection management that aren't
/// tied to any particular caller.
pub trait ErrorSink<E>: fmt::Debug + Send + Sync + 'static {
    /// Receive an error
    fn sink(&self, error: E);

    /// Clone this sink.
    fn boxed_clone(&self) -> Box<dyn ErrorSink<E>>;
}

/// An `ErrorSink` implementation that does nothing.
#[derive(Debug, Clone, Copy)]
pub struct NopErrorSink;

impl<E> ErrorSink<E> for NopErrorSink {
    fn sink(&self, _: E) {}

    fn boxed_clone(&self) -> Box<dyn ErrorSink<E>> {
        Box::new(self.clone())
    }
}

/// Information about the state of a `Pool`.
pub struct State {
    /// The number of connections currently being managed by the pool.
    pub connections: u32,
    /// The number of idle connections.
    pub idle_connections: u32,
    _p: (),
}

impl fmt::Debug for State {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("State")
            .field("connections", &self.connections)
            .field("idle_connections", &self.idle_connections)
            .finish()
    }
}

#[derive(Debug)]
struct Conn<C>
where
    C: Send,
{
    conn: C,
    birth: Instant,
}

struct IdleConn<C>
where
    C: Send,
{
    conn: Conn<C>,
    idle_start: Instant,
}

impl<C> IdleConn<C>
where
    C: Send,
{
    fn make_idle(conn: Conn<C>) -> IdleConn<C> {
        let now = Instant::now();
        IdleConn {
            conn: conn,
            idle_start: now,
        }
    }
}

/// A builder for a connection pool.
#[derive(Debug)]
pub struct Builder<M: ManageConnection> {
    /// The maximum number of connections allowed.
    max_size: u32,
    /// The minimum idle connection count the pool will attempt to maintain.
    min_idle: Option<u32>,
    /// Whether or not to test the connection on checkout.
    test_on_check_out: bool,
    /// The maximum lifetime, if any, that a connection is allowed.
    max_lifetime: Option<Duration>,
    /// The duration, if any, after which idle_connections in excess of `min_idle` are closed.
    idle_timeout: Option<Duration>,
    /// The duration to wait to start a connection before giving up.
    connection_timeout: Duration,
    /// The error sink.
    error_sink: Box<dyn ErrorSink<M::Error>>,
    /// The time interval used to wake up and reap connections.
    reaper_rate: Duration,
    _p: PhantomData<M>,
}

impl<M: ManageConnection> Default for Builder<M> {
    fn default() -> Self {
        Builder {
            max_size: 10,
            min_idle: None,
            test_on_check_out: true,
            max_lifetime: Some(Duration::from_secs(30 * 60)),
            idle_timeout: Some(Duration::from_secs(10 * 60)),
            connection_timeout: Duration::from_secs(30),
            error_sink: Box::new(NopErrorSink),
            reaper_rate: Duration::from_secs(30),
            _p: PhantomData,
        }
    }
}

impl<M: ManageConnection> Builder<M> {
    /// Constructs a new `Builder`.
    ///
    /// Parameters are initialized with their default values.
    pub fn new() -> Builder<M> {
        Default::default()
    }

    /// Sets the maximum number of connections managed by the pool.
    ///
    /// Defaults to 10.
    pub fn max_size(mut self, max_size: u32) -> Builder<M> {
        assert!(max_size > 0, "max_size must be greater than zero!");
        self.max_size = max_size;
        self
    }

    /// Sets the minimum idle connection count maintained by the pool.
    ///
    /// If set, the pool will try to maintain at least this many idle
    /// connections at all times, while respecting the value of `max_size`.
    ///
    /// Defaults to None.
    pub fn min_idle(mut self, min_idle: Option<u32>) -> Builder<M> {
        self.min_idle = min_idle;
        self
    }

    /// If true, the health of a connection will be verified through a call to
    /// `ManageConnection::is_valid` before it is provided to a pool user.
    ///
    /// Defaults to true.
    pub fn test_on_check_out(mut self, test_on_check_out: bool) -> Builder<M> {
        self.test_on_check_out = test_on_check_out;
        self
    }

    /// Sets the maximum lifetime of connections in the pool.
    ///
    /// If set, connections will be closed at the next reaping after surviving
    /// past this duration.
    ///
    /// If a connection reachs its maximum lifetime while checked out it will be
    /// closed when it is returned to the pool.
    ///
    /// Defaults to 30 minutes.
    pub fn max_lifetime(mut self, max_lifetime: Option<Duration>) -> Builder<M> {
        assert!(
            max_lifetime != Some(Duration::from_secs(0)),
            "max_lifetime must be greater than zero!"
        );
        self.max_lifetime = max_lifetime;
        self
    }

    /// Sets the idle timeout used by the pool.
    ///
    /// If set, idle connections in excess of `min_idle` will be closed at the
    /// next reaping after remaining idle past this duration.
    ///
    /// Defaults to 10 minutes.
    pub fn idle_timeout(mut self, idle_timeout: Option<Duration>) -> Builder<M> {
        assert!(
            idle_timeout != Some(Duration::from_secs(0)),
            "idle_timeout must be greater than zero!"
        );
        self.idle_timeout = idle_timeout;
        self
    }

    /// Sets the connection timeout used by the pool.
    ///
    /// Futures returned by `Pool::get` will wait this long before giving up and
    /// resolving with an error.
    ///
    /// Defaults to 30 seconds.
    pub fn connection_timeout(mut self, connection_timeout: Duration) -> Builder<M> {
        assert!(
            connection_timeout > Duration::from_secs(0),
            "connection_timeout must be non-zero"
        );
        self.connection_timeout = connection_timeout;
        self
    }

    /// Set the sink for errors that are not associated with any particular operation
    /// on the pool. This can be used to log and monitor failures.
    ///
    /// Defaults to `NopErrorSink`.
    pub fn error_sink(mut self, error_sink: Box<dyn ErrorSink<M::Error>>) -> Builder<M> {
        self.error_sink = error_sink;
        self
    }

    /// Used by tests
    #[allow(dead_code)]
    pub fn reaper_rate(mut self, reaper_rate: Duration) -> Builder<M> {
        self.reaper_rate = reaper_rate;
        self
    }

    fn build_inner(self, manager: M) -> (Pool<M>, impl Future<Item = (), Error = M::Error> + Send) {
        if let Some(min_idle) = self.min_idle {
            assert!(
                self.max_size >= min_idle,
                "min_idle must be no larger than max_size"
            );
        }

        let p = Pool::new_inner(self, manager);
        let f = p.replenish_idle_connections();
        (p, f)
    }

    /// Consumes the builder, returning a new, initialized `Pool`.
    ///
    /// The `Pool` will not be returned until it has established its configured
    /// minimum number of connections, or it times out.
    pub fn build(self, manager: M) -> impl Future<Item = Pool<M>, Error = M::Error> + Send {
        let (p, f) = self.build_inner(manager);
        f.map(|_| p)
    }

    /// Consumes the builder, returning a new, initialized `Pool`.
    ///
    /// Unlike `build`, this does not wait for any connections to be established
    /// before returning.
    pub fn build_unchecked(self, manager: M) -> Pool<M> {
        let (p, f) = self.build_inner(manager);
        p.spawn(p.sink_error(f));
        p
    }
}

/// The pool data that must be protected by a lock.
#[allow(missing_debug_implementations)]
struct PoolInternals<C>
where
    C: Send,
{
    waiters: VecDeque<oneshot::Sender<Conn<C>>>,
    conns: VecDeque<IdleConn<C>>,
    num_conns: u32,
    pending_conns: u32,
}

impl<C> PoolInternals<C>
where
    C: Send,
{
    fn put_idle_conn(&mut self, mut conn: IdleConn<C>) {
        loop {
            if let Some(waiter) = self.waiters.pop_front() {
                // This connection is no longer idle, send it back out.
                match waiter.send(conn.conn) {
                    Ok(_) => break,
                    // Oops, that receiver was gone. Loop and try again.
                    Err(c) => conn.conn = c,
                }
            } else {
                // Queue it in the idle queue.
                self.conns.push_back(conn);
                break;
            }
        }
    }
}

/// The guts of a `Pool`.
#[allow(missing_debug_implementations)]
struct SharedPool<M>
where
    M: ManageConnection + Send,
{
    statics: Builder<M>,
    manager: M,
    internals: Mutex<PoolInternals<M::Connection>>,
}

impl<M> SharedPool<M>
where
    M: ManageConnection,
{
    fn spawn<R>(&self, runnable: R)
    where
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static,
    {
        spawn(runnable.into_future());
    }

    fn sink_error<'a, E, F>(&self, f: F) -> impl Future<Item = F::Item, Error = ()> + Send + 'a
    where
        F: Future<Error = E> + Send + 'a,
        E: Into<M::Error>,
    {
        let sink = self.statics.error_sink.boxed_clone();
        f.map_err(move |e| sink.sink(e.into()))
    }

    fn or_timeout<'a, F>(
        &self,
        f: F,
    ) -> impl Future<Item = Option<F::Item>, Error = F::Error> + Send + 'a
    where
        F: IntoFuture + Send,
        F::Future: Send + 'a,
        F::Item: Send + 'a,
        F::Error: Send + ::std::fmt::Debug + 'a,
    {
        let runnable = f.into_future();
        Timeout::new(runnable, self.statics.connection_timeout).then(|r| match r {
            Ok(item) => Ok(Some(item)),
            Err(ref e) if e.is_elapsed() || e.is_timer() => Ok(None),
            Err(e) => Err(e.into_inner().unwrap()),
        })
    }
}

/// A generic connection pool.
pub struct Pool<M>
where
    M: ManageConnection,
{
    inner: Arc<SharedPool<M>>,
}

impl<M> Clone for Pool<M>
where
    M: ManageConnection,
{
    fn clone(&self) -> Self {
        Pool {
            inner: self.inner.clone(),
        }
    }
}

impl<M> fmt::Debug for Pool<M>
where
    M: ManageConnection,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!("Pool({:p})", self.inner))
    }
}

// Outside of Pool to avoid borrow splitting issues on self
// NB: This is called with the pool lock held.
fn add_connection<M>(
    pool: &Arc<SharedPool<M>>,
    internals: &mut PoolInternals<M::Connection>,
) -> impl Future<Item = (), Error = M::Error> + Send
where
    M: ManageConnection,
{
    assert!(internals.num_conns + internals.pending_conns < pool.statics.max_size);
    internals.pending_conns += 1;
    fn do_it<M>(pool: &Arc<SharedPool<M>>) -> impl Future<Item = (), Error = M::Error> + Send
    where
        M: ManageConnection,
    {
        let new_shared = Arc::downgrade(pool);
        let (tx, rx) = oneshot::channel();
        spawn(lazy(move || {
            match new_shared.upgrade() {
                None => Either::A(ok(())),
                Some(shared) => {
                    Either::B(shared.manager.connect().then(move |result| {
                        let mut locked = shared.internals.lock().unwrap();
                        match result {
                            Ok(conn) => {
                                let now = Instant::now();
                                let conn = IdleConn {
                                    conn: Conn {
                                        conn: conn,
                                        birth: now,
                                    },
                                    idle_start: now,
                                };
                                locked.pending_conns -= 1;
                                locked.num_conns += 1;
                                locked.put_idle_conn(conn);
                                tx.send(Ok(())).map_err(|_| ())
                            }
                            Err(err) => {
                                locked.pending_conns -= 1;
                                // TODO: retry?
                                tx.send(Err(err)).map_err(|_| ())
                            }
                        }
                    }))
                }
            }
        }));
        rx.then(|v| match v {
            Ok(o) => o,
            Err(_) => panic!(),
        })
    }

    do_it(pool)
}

fn get_idle_connection<M>(
    inner: Arc<SharedPool<M>>,
) -> impl Future<Item = Conn<M::Connection>, Error = Arc<SharedPool<M>>> + Send
where
    M: ManageConnection + Send,
    M::Connection: Send,
    M::Error: Send,
{
    loop_fn(inner, |inner| {
        let pool = inner.clone();
        let mut internals = inner.internals.lock().unwrap();
        if let Some(conn) = internals.conns.pop_front() {
            // Spin up a new connection if necessary to retain our minimum idle count
            if internals.num_conns + internals.pending_conns < pool.statics.max_size {
                let f = Pool::replenish_idle_connections_locked(&pool, &mut internals);
                pool.spawn(pool.sink_error(f));
            }

            // Go ahead and release the lock here.
            mem::drop(internals);

            if pool.statics.test_on_check_out {
                let birth = conn.conn.birth;
                Either::A(
                    pool.manager
                        .is_valid(conn.conn.conn)
                        .then(move |r| match r {
                            Ok(conn) => Ok(Loop::Break(Conn {
                                conn: conn,
                                birth: birth,
                            })),
                            Err((_, conn)) => {
                                {
                                    let mut locked = pool.internals.lock().unwrap();
                                    let _ = drop_connections(&pool, &mut locked, vec![conn]);
                                }
                                Ok(Loop::Continue(pool))
                            }
                        }),
                )
            } else {
                Either::B(Ok(Loop::Break(conn.conn)).into_future())
            }
        } else {
            Either::B(Err(pool).into_future())
        }
    })
}

// Drop connections
// NB: This is called with the pool lock held.
fn drop_connections<'a, L, M>(
    pool: &Arc<SharedPool<M>>,
    mut internals: L,
    to_drop: Vec<M::Connection>,
) -> Box<dyn Future<Item = (), Error = M::Error> + Send>
where
    L: BorrowMut<MutexGuard<'a, PoolInternals<M::Connection>>>,
    M: ManageConnection,
{
    let internals = internals.borrow_mut();

    internals.num_conns -= to_drop.len() as u32;
    // We might need to spin up more connections to maintain the idle limit, e.g.
    // if we hit connection lifetime limits
    let f = if internals.num_conns + internals.pending_conns < pool.statics.max_size {
        Either::A(Pool::replenish_idle_connections_locked(
            pool,
            &mut *internals,
        ))
    } else {
        Either::B(ok(()))
    };

    // Maybe unlock. If we're passed a MutexGuard, this will unlock. If we're passed a
    // &mut MutexGuard it won't.
    mem::drop(internals);

    // And drop the connections
    // TODO: connection_customizer::on_release! That would require figuring out the
    // locking situation though
    Box::new(f)
}

fn drop_idle_connections<'a, M>(
    pool: &Arc<SharedPool<M>>,
    internals: MutexGuard<'a, PoolInternals<M::Connection>>,
    to_drop: Vec<IdleConn<M::Connection>>,
) -> Box<dyn Future<Item = (), Error = M::Error> + Send>
where
    M: ManageConnection,
{
    let to_drop = to_drop.into_iter().map(|c| c.conn.conn).collect();
    drop_connections(pool, internals, to_drop)
}

// Reap connections if necessary.
// NB: This is called with the pool lock held.
fn reap_connections<'a, M>(
    pool: &Arc<SharedPool<M>>,
    mut internals: MutexGuard<'a, PoolInternals<M::Connection>>,
) -> impl Future<Item = (), Error = M::Error> + Send
where
    M: ManageConnection,
{
    let now = Instant::now();
    let (to_drop, preserve) = internals.conns.drain(..).partition2(|conn| {
        let mut reap = false;
        if let Some(timeout) = pool.statics.idle_timeout {
            reap |= now - conn.idle_start >= timeout;
        }
        if let Some(lifetime) = pool.statics.max_lifetime {
            reap |= now - conn.conn.birth >= lifetime;
        }
        reap
    });
    internals.conns = preserve;
    drop_idle_connections(pool, internals, to_drop)
}

fn schedule_one_reaping<M>(
    pool: &SharedPool<M>,
    interval: Interval,
    weak_shared: Weak<SharedPool<M>>,
) where
    M: ManageConnection,
{
    pool.spawn(
        interval
            .into_future()
            .map_err(|_| ())
            .and_then(move |(_, interval)| match weak_shared.upgrade() {
                None => Either::A(ok(())),
                Some(shared) => {
                    let shared2 = shared.clone();
                    let locked = shared.internals.lock().unwrap();
                    Either::B(
                        shared
                            .sink_error(reap_connections(&shared, locked))
                            .then(move |r| {
                                schedule_one_reaping(&shared2, interval, weak_shared);
                                r
                            }),
                    )
                }
            }),
    )
}

impl<M: ManageConnection> Pool<M> {
    fn new_inner(builder: Builder<M>, manager: M) -> Pool<M> {
        let internals = PoolInternals {
            waiters: VecDeque::new(),
            conns: VecDeque::new(),
            num_conns: 0,
            pending_conns: 0,
        };

        let shared = Arc::new(SharedPool {
            statics: builder,
            manager: manager,
            internals: Mutex::new(internals),
        });

        if shared.statics.max_lifetime.is_some() || shared.statics.idle_timeout.is_some() {
            let s = Arc::downgrade(&shared);
            spawn(lazy(|| {
                s.upgrade().ok_or(()).map(|shared| {
                    let interval = Interval::new_interval(shared.statics.reaper_rate);
                    schedule_one_reaping(&shared, interval, s);
                })
            }))
        }

        Pool { inner: shared }
    }

    fn spawn<R>(&self, runnable: R)
    where
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static,
    {
        self.inner.spawn(runnable);
    }

    fn sink_error<'a, E, F>(&self, f: F) -> impl Future<Item = F::Item, Error = ()> + Send + 'a
    where
        F: Future<Error = E> + Send + 'a,
        E: Into<M::Error> + 'a,
    {
        self.inner.sink_error(f)
    }

    fn replenish_idle_connections_locked(
        pool: &Arc<SharedPool<M>>,
        internals: &mut PoolInternals<M::Connection>,
    ) -> impl Future<Item = (), Error = M::Error> + Send {
        let slots_available = pool.statics.max_size - internals.num_conns - internals.pending_conns;
        let idle = internals.conns.len() as u32;
        let desired = pool.statics.min_idle.unwrap_or(0);
        let f = FuturesUnordered::from_iter(
            (idle..max(idle, min(desired, idle + slots_available)))
                .map(|_| add_connection(pool, internals)),
        );
        f.fold((), |_, _| Ok(()))
    }

    fn replenish_idle_connections(&self) -> impl Future<Item = (), Error = M::Error> + Send {
        let mut locked = self.inner.internals.lock().unwrap();
        Pool::replenish_idle_connections_locked(&self.inner, &mut locked)
    }

    /// Returns a `Builder` instance to configure a new pool.
    pub fn builder() -> Builder<M> {
        Builder::new()
    }

    /// Returns information about the current state of the pool.
    pub fn state(&self) -> State {
        let locked = self.inner.internals.lock().unwrap();
        State {
            connections: locked.num_conns,
            idle_connections: locked.conns.len() as u32,
            _p: (),
        }
    }

    /// Run a closure with a `Connection`.
    ///
    /// The closure will be executed on the tokio event loop provided during
    /// the construction of this pool, so it must be `Send`. The closure's return
    /// value is also `Send` so that the Future can be consumed in contexts where
    /// `Send` is needed.
    ///
    /// # Futures 0.3 + Async/Await
    ///
    /// In order to use this with Futures 0.3 + async/await syntax, use `.boxed().compat()` on the inner future in order to convert it to a version 0.1 Future.
    ///
    /// ```ignore
    /// // Note that this version of `futures` is 0.3
    /// use futures::compat::{Future01CompatExt}; // trait provides `.compat()`
    ///
    /// async fn future_03_example(client: Client) -> Result<(String, Client), (Error, Client)> {
    ///     Ok(("Example".to_string(), client))
    /// }
    ///
    /// client.run(|client| {
    ///     future_03_example(client).boxed().compat()
    /// })
    /// ```
    pub fn run<'a, T, E, U, F>(
        &self,
        f: F,
    ) -> impl Future<Item = T, Error = RunError<E>> + Send + 'a
    where
        F: FnOnce(M::Connection) -> U + Send + 'a,
        U: IntoFuture<Item = (T, M::Connection), Error = (E, M::Connection)> + Send + 'a,
        U::Future: Send + 'a,
        E: From<M::Error> + Send + 'a,
        T: Send + 'a,
    {
        let inner = self.inner.clone();
        let inner2 = inner.clone();
        lazy(move || {
            get_idle_connection(inner).then(move |r| match r {
                Ok(conn) => Either::A(ok(conn)),
                Err(inner) => {
                    let (tx, rx) = oneshot::channel();
                    {
                        let mut locked = inner.internals.lock().unwrap();
                        locked.waiters.push_back(tx);
                        if locked.num_conns + locked.pending_conns < inner.statics.max_size {
                            let f = add_connection(&inner, &mut locked);
                            inner.spawn(inner.sink_error(f));
                        }
                    }

                    Either::B(inner.or_timeout(rx).then(move |r| match r {
                        Ok(Some(conn)) => Ok(conn),
                        _ => Err(RunError::TimedOut),
                    }))
                }
            })
        })
        .and_then(|conn| {
            let inner = inner2;
            let birth = conn.birth;
            f(conn.conn)
                .into_future()
                .then(move |r| {
                    let (r, mut conn): (Result<_, E>, _) = match r {
                        Ok((t, conn)) => (Ok(t), conn),
                        Err((e, conn)) => (Err(e.into()), conn),
                    };
                    // Supposed to be fast, but do it before locking anyways.
                    let broken = inner.manager.has_broken(&mut conn);

                    let mut locked = inner.internals.lock().unwrap();
                    if broken {
                        let _ = drop_connections(&inner, locked, vec![conn]);
                    } else {
                        let conn = IdleConn::make_idle(Conn {
                            conn: conn,
                            birth: birth,
                        });
                        locked.put_idle_conn(conn);
                    }
                    r
                })
                .map_err(|e| RunError::User(e))
        })
    }

    /// Get a new dedicated connection that will not be managed by the pool.
    /// An application may want a persistent connection (e.g. to do a
    /// postgres LISTEN) that will not be closed or repurposed by the pool.
    ///
    /// This method allows reusing the manager's configuration but otherwise
    /// bypassing the pool
    pub fn dedicated_connection(
        &self,
    ) -> impl Future<Item = M::Connection, Error = M::Error> + Send {
        let inner = self.inner.clone();
        inner.manager.connect()
    }
}
