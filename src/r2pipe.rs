//! Provides functionality to connect with radare2.
//!
//! Please check crate level documentation for more details and example.

use crate::{Error, Result};

#[cfg(feature = "http")]
use reqwest;

use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufReader;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process;
use std::process::Command;
use std::process::Stdio;
use std::str;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use serde_json::Value;

/// File descriptors to the parent r2 process.
pub struct R2PipeLang {
    read: BufReader<File>,
    write: File,
}

/// Stores descriptors to the spawned r2 process.
pub struct R2PipeSpawn {
    read: BufReader<process::ChildStdout>,
    write: process::ChildStdin,
}

/// Stores the socket address of the r2 process.
pub struct R2PipeTcp {
    socket_addr: SocketAddr,
}

#[cfg(feature = "http")]
#[cfg_attr(doc_cfg, doc(cfg(feature = "http")))]
pub struct R2PipeHttp {
    host: String,
}

/// Stores thread metadata
/// It stores both a sending and receiving end to the thread, allowing convenient interaction
/// So we can send commands using R2PipeThread::send() and fetch outputs using R2PipeThread::recv()
pub struct R2PipeThread {
    r2recv: mpsc::Receiver<String>,
    r2send: mpsc::Sender<String>,
    pub id: u16,
    pub handle: thread::JoinHandle<Result<()>>,
}

#[derive(Default, Clone)]
pub struct R2PipeSpawnOptions {
    pub exepath: String,
    pub args: Vec<&'static str>,
}

/// Provides abstraction between the three invocation methods.
pub enum R2Pipe {
    Pipe(R2PipeSpawn),
    Lang(R2PipeLang),
    Tcp(R2PipeTcp),
    #[cfg(feature = "http")]
    #[cfg_attr(doc_cfg, doc(cfg(feature = "http")))]
    Http(R2PipeHttp),
}

fn atoi(k: &str) -> i32 {
    k.parse::<i32>().unwrap_or(-1)
}

fn getenv(k: &str) -> i32 {
    match env::var(k) {
        Ok(val) => atoi(&val),
        Err(_) => -1,
    }
}

fn process_result(res: Vec<u8>) -> Result<String> {
    let len = res.len();
    if len == 0 {
        Err(Error::EmptyResponse)
    } else {
        Ok(str::from_utf8(&res[..len - 1])?.to_string())
    }
}

#[macro_export]
macro_rules! open_pipe {
	() => {
            R2Pipe::open(),
        };
	($x: expr) => {
		match $x {
			Some(path) => R2Pipe::spawn(path, None),
			None => R2Pipe::open(),
		}
	};
	($x: expr, $y: expr) => {
		match $x $y {
			Some(path, opts) => R2Pipe::spawn(path, opts),
			(None, None) => R2Pipe::open(),
		}
	}
}

impl R2Pipe {
    #[cfg(not(windows))]
    pub fn open() -> Result<R2Pipe> {
        use std::os::unix::io::FromRawFd;

        let (f_in, f_out) = R2Pipe::in_session().ok_or(Error::NoSession)?;

        let res = unsafe {
            // dup file descriptors to avoid from_raw_fd ownership issue
            let (d_in, d_out) = (libc::dup(f_in), libc::dup(f_out));
            R2PipeLang {
                read: BufReader::new(File::from_raw_fd(d_in)),
                write: File::from_raw_fd(d_out),
            }
        };
        Ok(R2Pipe::Lang(res))
    }

    #[cfg(windows)]
    pub fn open() -> Result<R2Pipe> {
        unimplemented!()
    }

    pub fn cmd(&mut self, cmd: &str) -> Result<String> {
        match *self {
            R2Pipe::Pipe(ref mut x) => x.cmd(cmd.trim()),
            R2Pipe::Lang(ref mut x) => x.cmd(cmd.trim()),
            R2Pipe::Tcp(ref mut x) => x.cmd(cmd.trim()),
            #[cfg(feature = "http")]
            R2Pipe::Http(ref mut x) => x.cmd(cmd.trim()),
        }
    }

    pub fn cmdj(&mut self, cmd: &str) -> Result<Value> {
        match *self {
            R2Pipe::Pipe(ref mut x) => x.cmdj(cmd.trim()),
            R2Pipe::Lang(ref mut x) => x.cmdj(cmd.trim()),
            R2Pipe::Tcp(ref mut x) => x.cmdj(cmd.trim()),
            #[cfg(feature = "http")]
            R2Pipe::Http(ref mut x) => x.cmdj(cmd.trim()),
        }
    }

    pub fn close(&mut self) {
        match *self {
            R2Pipe::Pipe(ref mut x) => x.close(),
            R2Pipe::Lang(ref mut x) => x.close(),
            R2Pipe::Tcp(ref mut x) => x.close(),
            #[cfg(feature = "http")]
            R2Pipe::Http(ref mut x) => x.close(),
        }
    }

    pub fn in_session() -> Option<(i32, i32)> {
        let f_in = getenv("R2PIPE_IN");
        let f_out = getenv("R2PIPE_OUT");
        if f_in < 0 || f_out < 0 {
            return None;
        }
        Some((f_in, f_out))
    }

    #[cfg(windows)]
    pub fn in_windows_session() -> Option<String> {
        match env::var("R2PIPE_PATH") {
            Ok(val) => Some(format!("\\\\.\\pipe\\{}", val)),
            Err(_) => None,
        }
    }

    /// Creates a new R2PipeSpawn.
    pub fn spawn<T: AsRef<str>>(name: T, opts: Option<R2PipeSpawnOptions>) -> Result<R2Pipe> {
        if name.as_ref() == "" && R2Pipe::in_session().is_some() {
            return R2Pipe::open();
        }

        let exepath = match opts {
            Some(ref opt) => opt.exepath.clone(),
            _ => "r2".to_owned(),
        };
        let args = match opts {
            Some(ref opt) => opt.args.clone(),
            _ => vec![],
        };
        let path = Path::new(name.as_ref());
        let child = Command::new(exepath)
            .arg("-q0")
            .args(&args)
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        // If stdin/stdout is not available, hard error
        let sin = child.stdin.unwrap();
        let mut sout = child.stdout.unwrap();

        // flush out the initial null byte.
        let mut w = [0; 1];
        sout.read_exact(&mut w)?;

        let res = R2PipeSpawn {
            read: BufReader::new(sout),
            write: sin,
        };

        Ok(R2Pipe::Pipe(res))
    }

    /// Creates a new R2PipeTcp
    pub fn tcp<A: ToSocketAddrs>(addr: A) -> Result<R2Pipe> {
        // use `connect` to figure out which socket address works
        let stream = TcpStream::connect(addr)?;
        let addr = stream.peer_addr()?;
        Ok(R2Pipe::Tcp(R2PipeTcp { socket_addr: addr }))
    }

    #[cfg(feature = "http")]
    #[cfg_attr(doc_cfg, doc(cfg(feature = "http")))]
    /// Creates a new R2PipeHttp
    pub fn http(host: &str) -> R2Pipe {
        R2Pipe::Http(R2PipeHttp {
            host: host.to_string(),
        })
    }

    /// Creates new pipe threads
    /// First two arguments for R2Pipe::threads() are the same as for R2Pipe::spawn() but inside vectors
    /// Third and last argument is an option to a callback function
    /// The callback function takes two Arguments: Thread ID and r2pipe output
    pub fn threads(
        names: Vec<&'static str>,
        opts: Vec<Option<R2PipeSpawnOptions>>,
        callback: Option<Arc<dyn Fn(u16, String) + Sync + Send>>,
    ) -> Result<Vec<R2PipeThread>> {
        if names.len() != opts.len() {
            return Err(Error::ArgumentMismatch);
        }

        let mut pipes = Vec::new();

        for n in 0..names.len() {
            let (htx, rx) = mpsc::channel();
            let (tx, hrx) = mpsc::channel();
            let name = names[n];
            let opt = opts[n].clone();
            let cb = callback.clone();
            let t = thread::spawn(move || -> Result<()> {
                let mut r2 = R2Pipe::spawn(name, opt)?;
                loop {
                    let cmd: String = hrx.recv()?;
                    if cmd == "q" {
                        break;
                    }
                    let res = r2.cmdj(&cmd)?.to_string();
                    htx.send(res.clone())?;
                    if let Some(cbs) = cb.clone() {
                        thread::spawn(move || {
                            cbs(n as u16, res);
                        });
                    };
                }
                Ok(())
            });
            pipes.push(R2PipeThread {
                r2recv: rx,
                r2send: tx,
                id: n as u16,
                handle: t,
            });
        }
        Ok(pipes)
    }
}

impl R2PipeThread {
    pub fn send(&self, cmd: String) -> Result<()> {
        Ok(self.r2send.send(cmd)?)
    }

    pub fn recv(&self, block: bool) -> Result<String> {
        if block {
            Ok(self.r2recv.recv()?)
        } else {
            Ok(self.r2recv.try_recv()?)
        }
    }
}

impl R2PipeSpawn {
    pub fn cmd(&mut self, cmd: &str) -> Result<String> {
        let cmd = cmd.to_owned() + "\n";
        self.write.write_all(cmd.as_bytes())?;

        let mut res: Vec<u8> = Vec::new();
        self.read.read_until(0u8, &mut res)?;
        process_result(res)
    }

    pub fn cmdj(&mut self, cmd: &str) -> Result<Value> {
        let result = self.cmd(cmd)?;
        if result.is_empty() {
            return Err(Error::EmptyResponse);
        }
        Ok(serde_json::from_str(&result)?)
    }

    pub fn close(&mut self) {
        let _ = self.cmd("q!");
    }
}

impl R2PipeLang {
    pub fn cmd(&mut self, cmd: &str) -> Result<String> {
        self.write.write_all(cmd.as_bytes())?;
        let mut res: Vec<u8> = Vec::new();
        self.read.read_until(0u8, &mut res)?;
        process_result(res)
    }

    pub fn cmdj(&mut self, cmd: &str) -> Result<Value> {
        let res = self.cmd(cmd)?;

        Ok(serde_json::from_str(&res)?)
    }

    pub fn close(&mut self) {
        // self.read.close();
        // self.write.close();
    }
}

#[cfg(feature = "http")]
#[cfg_attr(doc_cfg, doc(cfg(feature = "http")))]
impl R2PipeHttp {
    pub fn cmd(&mut self, cmd: &str) -> Result<String> {
        let url = format!("http://{}/cmd/{}", self.host, cmd);
        let res = reqwest::get(&url)?;
        let bytes = res.bytes().filter_map(|e| e.ok()).collect::<Vec<_>>();
        Ok(str::from_utf8(bytes.as_slice()).map(|s| s.to_string())?)
    }

    pub fn cmdj(&mut self, cmd: &str) -> Result<Value> {
        let res = self.cmd(cmd)?;
        Ok(serde_json::from_str(&res)?)
    }

    pub fn close(&mut self) {}
}

impl R2PipeTcp {
    pub fn cmd(&mut self, cmd: &str) -> Result<String> {
        let mut stream = TcpStream::connect(self.socket_addr)?;
        stream.write_all(cmd.as_bytes())?;
        let mut res: Vec<u8> = Vec::new();
        stream.read_to_end(&mut res)?;
        res.push(0);
        process_result(res)
    }

    pub fn cmdj(&mut self, cmd: &str) -> Result<Value> {
        let res = self.cmd(cmd)?;
        Ok(serde_json::from_str(&res)?)
    }

    pub fn close(&mut self) {}
}
