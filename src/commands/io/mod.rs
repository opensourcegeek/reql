mod pool;
mod request;
mod handshake;


use {Client, Config, Connection, Document, IntoArg, Opts, Request, Response, Result, Run, Server,
     Session, SessionManager};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use errors::*;
use futures::{Async, Poll, Sink, Stream};
use futures::sync::mpsc;
use ordermap::OrderMap;
use parking_lot::RwLock;
use protobuf::ProtobufEnum;
use ql2::proto::{Datum, Term};
use ql2::proto::Query_QueryType as QueryType;
use r2d2;
use reql_types::{Change, ServerStatus};
use serde::de::DeserializeOwned;
use slog::Logger;
use std::{error, thread};
use std::cmp::Ordering;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tokio_core::reactor::Remote;
use uuid::Uuid;

lazy_static! {
    static ref CONFIG: RwLock<OrderMap<Connection, Config>> = RwLock::new(OrderMap::new());
    static ref POOL: RwLock<OrderMap<Connection, r2d2::Pool<SessionManager>>> = RwLock::new(OrderMap::new());
}

const CHANNEL_SIZE: usize = 1024;

pub fn connect<A: IntoArg>(client: &Client, args: A) -> Result<Connection>
{
    if let Err(ref error) = client.term {
        return Err(error.clone());
    }
    let arg = args.into_arg();
    let aterm = arg.term?;
    let conn = Connection(Uuid::new_v4());
    let logger = client.logger.new(o!("command" => "connect"));
    let query = format!("{}.connect({})", client.query, arg.string);
    debug!(logger, "{}", query);
    info!(logger, "creating connection pool...");
    match arg.remote {
        Some(remote) => conn.set_config(aterm, remote, logger.clone())?,
        None => {
            return Err(io_error("a futures handle is required for `connect`"))?;
        }
    }
    conn.set_latency()?;
    let config = r2d2::Config::builder()
        .pool_size(144)
        .idle_timeout(Some(Duration::from_secs(120)))
        .max_lifetime(Some(Duration::from_secs(86400)))
        .min_idle(Some(5))
        .connection_timeout(Duration::from_secs(90))
        .build();
    let session = SessionManager(conn);
    let r2d2 = r2d2::Pool::new(config, session)
        .map_err(|err| io_error(err))?;
    conn.set_pool(r2d2);
    info!(logger, "connection pool created successfully");
    conn.maintain();
    Ok(conn)
}

impl<A: IntoArg> Run<A> for Client
{
    fn run<T: DeserializeOwned + Send + 'static>(&self, args: A) -> Result<Response<T>>
    {
        let cterm = match self.term {
            Ok(ref term) => term.clone(),
            Err(ref error) => {
                return Err(error.clone());
            }
        };
        let arg = args.into_arg();
        let aterm = arg.term?;
        let logger = self.logger.new(o!("command" => "run"));
        let query = format!("{}.run({})", self.query, arg.string);
        debug!(logger, "{}", query);
        let conn = match arg.pool {
            Some(conn) => conn.clone(),
            None => {
                let msg = String::from("`run` requires a connection");
                return Err(DriverError::Other(msg))?;
            }
        };
        let pool = match POOL.read().get(&conn) {
            Some(pool) => pool.clone(),
            None => {
                let msg = String::from("bug: connection not in POOL");
                return Err(DriverError::Other(msg))?;
            }
        };
        let cfg = match CONFIG.read().get(&conn) {
            Some(cfg) => cfg.clone(),
            None => {
                return Err(io_error("a tokio handle is required"))?;
            }
        };
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        //let remote = cfg.remote.clone();
        // @TODO spawning a thread per query is less than ideal. Ideally we will
        // need first class support for Tokio to get rid of this.
        ::std::thread::spawn(move || {
                                 let req = Request {
                                     term: cterm,
                                     opts: aterm,
                                     pool: pool,
                                     cfg: cfg,
                                     tx: tx,
                                     write: true,
                                     retry: false,
                                     logger: logger,
                                 };
                                 req.submit();
                             });
        Ok(Response {
               done: false,
               rx: rx,
           })
    }
}

impl<T: DeserializeOwned + Send> Stream for Response<T>
{
    type Item = Option<Document<T>>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error>
    {
        if self.done {
            return Ok(Async::Ready(None));
        }
        match self.rx.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(Some(res))) => {
                match res {
                    Ok(data) => Ok(Async::Ready(Some(data))),
                    Err(error) => Err(error),
                }
            }
            Ok(Async::Ready(None)) => {
                self.done = true;
                Ok(Async::Ready(None))
            }
            Err(_) => {
                self.done = true;
                let msg = String::from("an error occured while processing the stream");
                Err(DriverError::Other(msg))?
            }
        }
    }
}

fn io_error<T>(err: T) -> io::Error
    where T: Into<Box<error::Error + Send + Sync>>
{
    io::Error::new(io::ErrorKind::Other, err)
}

impl Default for Opts
{
    fn default() -> Opts
    {
        Opts {
            db: "test".into(),
            user: "admin".into(),
            password: String::new(),
            // @TODO number of retries doesn't mean much
            // let's use a timeout instead and make it an
            // option in both connect and run. The connect
            // one will be the user default and the run one
            // will have the highest precedence. Also let's
            // call it `retry_timeout` to communicate clearly
            // what it does.
            retries: 5,
            reproducible: false,
            tls: None,
        }
    }
}

impl Ord for Server
{
    fn cmp(&self, other: &Server) -> Ordering
    {
        self.latency.cmp(&other.latency)
    }
}

impl PartialOrd for Server
{
    fn partial_cmp(&self, other: &Server) -> Option<Ordering>
    {
        Some(self.cmp(other))
    }
}

impl PartialEq for Server
{
    fn eq(&self, other: &Server) -> bool
    {
        self.latency == other.latency
    }
}

fn find_datum(mut term: Term) -> Vec<Datum>
{
    let mut res = Vec::new();
    if term.has_datum() {
        res.push(term.take_datum());
    } else {
        for term in term.take_args().into_vec() {
            for datum in find_datum(term) {
                res.push(datum);
            }
        }
    }
    res
}

fn take_string(key: &str, val: Vec<Datum>) -> Result<String>
{
    for mut datum in val {
        return Ok(datum.take_r_str());
    }
    Err(DriverError::Other(format!("`{}` must be a string", key)))?
}

fn take_bool(key: &str, val: Vec<Datum>) -> Result<bool>
{
    for datum in val {
        return Ok(datum.get_r_bool());
    }
    Err(DriverError::Other(format!("`{}` must be a boolean", key)))?
}

impl Connection
{
    fn set_config(&self, mut term: Term, remote: Remote, logger: Logger) -> Result<()>
    {
        let mut cluster = OrderMap::new();
        let mut hosts = Vec::new();
        let mut opts = Opts::default();

        let optargs = term.take_optargs().into_vec();
        for mut arg in optargs {
            let key = arg.take_key();
            let val = find_datum(arg.take_val());

            if key == "db" {
                opts.db = take_string(&key, val)?;
            } else if key == "user" {
                opts.user = take_string(&key, val)?;
            } else if key == "password" {
                opts.password = take_string(&key, val)?;
            } else if key == "reproducible" {
                opts.reproducible = take_bool(&key, val)?;
            } else if key == "servers" {
                for host in val {
                    hosts.push(take_string(&key, vec![host])?);
                }
            }
        }

        if hosts.is_empty() {
            hosts.push("localhost".into());
        }

        for host in hosts {
            let addresses = host.to_socket_addrs()
                .or_else(|_| {
                             let host = format!("{}:{}", host, 28015);
                             host.to_socket_addrs()
                         })?;
            let server = Server::new(&host, addresses.collect());
            cluster.insert(host, server);
        }

        CONFIG
            .write()
            .insert(*self,
                    Config {
                        cluster: cluster,
                        opts: opts,
                        remote: remote,
                        logger: logger,
                    });

        Ok(())
    }

    fn maintain(&self)
    {
        self.reset_cluster();
        let conn = *self;
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        thread::spawn(move || {
                          let r = Client::new();
                          let query = r.db("rethinkdb")
                              .table("server_status")
                              .changes()
                              .with_args(args!({include_initial: true}));
                          loop {
                              let changes = query
                                  .run::<Change<ServerStatus, ServerStatus>>(conn)
                                  .unwrap();
                              for change in changes.wait() {
                                  match change {
                                      Ok(Some(Document::Expected(change))) => {
                                          if let Some(ref mut config) =
                        CONFIG.write().get_mut(&conn) {
                                              let cluster = &mut config.cluster;
                                              if let Some(status) = change.new_val {
                                                  let mut addresses = Vec::new();
                                                  for addr in status.network.canonical_addresses {
                                                      let socket =
                                                          SocketAddr::new(addr.host,
                                                                          status.network.reql_port);
                                                      addresses.push(socket);
                                                  }
                                                  let mut server = Server::new(&status.name,
                                                                               addresses);
                                                  server.set_latency();
                                                  cluster.insert(server.name.to_owned(), server);
                                                  let _ = tx.clone().send(());
                                              } else if let Some(status) = change.old_val {
                                                  cluster.remove(&status.name);
                                              }
                                          }
                                      }
                                      Ok(res) => {
                        println!("unexpected response from server: {:?}", res);
                    }
                                      Err(error) => {
                        println!("{:?}", error);
                    }
                                  }
                              }
                              thread::sleep(Duration::from_millis(500));
                          }
                      });
        // wait for at least one database result before continuing
        let _ = rx.wait();
    }

    fn reset_cluster(&self)
    {
        if let Some(ref mut config) = CONFIG.write().get_mut(self) {
            config.cluster = OrderMap::new();
        }
    }

    fn set_latency(&self) -> Result<()>
    {
        match CONFIG.write().get_mut(self) {
            Some(ref mut config) => {
                for mut server in config.cluster.values_mut() {
                    server.set_latency();
                }
                Ok(())
            }
            None => {
                let msg = String::from("conn.set_latency() called before setting configuration");
                Err(DriverError::Other(msg))?
            }
        }
    }

    fn config(&self) -> Config
    {
        CONFIG.read().get(self).unwrap().clone()
    }

    fn set_pool(&self, pool: r2d2::Pool<SessionManager>)
    {
        POOL.write().insert(*self, pool);
    }
}

impl Server
{
    fn new(host: &str, addresses: Vec<SocketAddr>) -> Server
    {
        Server {
            name: host.to_string(),
            addresses: addresses,
            latency: Duration::from_millis(u64::max_value()),
        }
    }

    fn set_latency(&mut self)
    {
        for address in self.addresses.iter() {
            let start = Instant::now();
            if let Ok(_) = TcpStream::connect(address) {
                self.latency = start.elapsed();
                break;
            }
        }
    }
}

fn write_query(conn: &mut Session, query: &str) -> Result<()>
{
    let query = query.as_bytes();
    let token = conn.id;
    if let Err(error) = conn.stream.write_u64::<LittleEndian>(token) {
        conn.broken = true;
        return Err(io_error(error))?;
    }
    if let Err(error) = conn.stream.write_u32::<LittleEndian>(query.len() as u32) {
        conn.broken = true;
        return Err(io_error(error))?;
    }
    if let Err(error) = conn.stream.write_all(query) {
        conn.broken = true;
        return Err(io_error(error))?;
    }
    if let Err(error) = conn.stream.flush() {
        conn.broken = true;
        return Err(io_error(error))?;
    }
    Ok(())
}

fn read_query(conn: &mut Session) -> Result<Vec<u8>>
{
    let _ = match conn.stream.read_u64::<LittleEndian>() {
        Ok(token) => token,
        Err(error) => {
            conn.broken = true;
            return Err(io_error(error))?;
        }
    };
    let len = match conn.stream.read_u32::<LittleEndian>() {
        Ok(len) => len,
        Err(error) => {
            conn.broken = true;
            return Err(io_error(error))?;
        }
    };
    let mut resp = vec![0u8; len as usize];
    if let Err(error) = conn.stream.read_exact(&mut resp) {
        conn.broken = true;
        return Err(io_error(error))?;
    }
    Ok(resp)
}

fn wrap_query(query_type: QueryType, query: Option<String>, options: Option<String>) -> String
{
    let mut qry = format!("[{}", query_type.value());
    if let Some(query) = query {
        qry.push_str(&format!(",{}", query));
    }
    if let Some(options) = options {
        qry.push_str(&format!(",{}", options));
    }
    qry.push_str("]");
    qry
}
