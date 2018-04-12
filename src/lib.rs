/*
 * Copyright (c) 2018 Boucher, Antoni <bouanto@zoho.com>
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy of
 * this software and associated documentation files (the "Software"), to deal in
 * the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do so,
 * subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
 * FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
 * COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
 * IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

/*
 * TODO: Use a Pin to make it safer.
 * TODO: Add combinator functions like one to get the first result between two awaitable async
 * calls or collect both results.
 */

#![feature(generators, generator_trait)]

use std::cell::Cell;
use std::io::{
    self,
    Read,
    Write,
};
use std::io::ErrorKind::WouldBlock;
use std::mem;
use std::net::{self, SocketAddr, ToSocketAddrs};
use std::ops::{Deref, DerefMut, Generator, GeneratorState};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::raw::{c_int, c_void};

use self::YieldValue::{Fd, Task};

// TODO: create a BitSet and use as a key for a Map<BitSet, Generator>.
const FD_COUNT: usize = 64;

const EPOLL_CTL_ADD: c_int = 1;
const EPOLLIN: c_int = 0x1;

thread_local! {
    static PIPE_WRITER_FD: Cell<RawFd> = Cell::new(-1);
}

type GenTask = Box<Generator<Yield = YieldValue, Return = ()>>;

pub enum YieldValue {
    Fd(RawFd),
    Task(GenTask),
}

const O_NONBLOCK: c_int = 2048;

#[repr(C)]
#[derive(Clone, Copy)]
struct epoll_event {
    pub events: u32,
    pub u64: u64,
}

extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;

    fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

#[macro_export]
macro_rules! await {
    ($call:expr) => {{
        let mut generator = $call;
        let result;
        loop {
            match unsafe { generator.resume() } {
                GeneratorState::Yielded(fd) => yield fd,
                GeneratorState::Complete(value) => {
                    result = value;
                    break;
                },
            }
        }
        result
    }};
}

#[macro_export]
macro_rules! async {
    (pub fn $func_name:ident (& $this:ident $(, $arg_names:ident : $(& $arg_types:ident)*)* ) -> $ret_typ:ty $block:block) => {
        pub fn $func_name<'a>(&'a $this $(, $arg_names: $(&'a $arg_types)*)*) ->
            impl Generator<Yield = YieldValue, Return = $ret_typ> + 'a
        {
            move || {
                $block
            }
        }
    };
}

#[macro_export]
macro_rules! handle_would_block {
    ($object:ident . $func_name:ident ($($args:expr),*), |$pat:pat| $converter:expr) => {
        loop {
            match $object.$func_name($($args),*) {
                Err(error) => {
                    if error.kind() == WouldBlock {
                        yield Fd($object.as_raw_fd());
                    }
                    else {
                        return Err(error);
                    }
                },
                Ok(result) => {
                    let $pat = result;
                    return Ok($converter)
                },
            }
        }
    };
    ($object:ident . $func_name:ident ($($args:expr),*)) => {
        handle_would_block!($object.$func_name($($args),*), |res| res)
    };
}

#[macro_export]
macro_rules! spawn {
    ($($tts:tt)*) => {{
        yield Task(Box::new(static move || { $($tts)* }));
        let write_pipe = $crate::EventLoop::write_pipe();
        await!(write_pipe.write_async(b"0"))
    }};
}

struct Pipe;

struct ReadPipe {
    fd: RawFd,
}

impl ReadPipe {
    fn new(fd: RawFd) -> Self {
        Self {
            fd,
        }
    }
}

// TODO: Create another trait AsyncFd to avoid being able to use blocking IO by mistake.
impl AsRawFd for ReadPipe {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

pub struct WritePipe {
    fd: RawFd,
}

impl AsRawFd for WritePipe {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl WritePipe {
    fn new(fd: RawFd) -> Self {
        Self {
            fd,
        }
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let result = unsafe { write(self.fd, buf.as_ptr() as *const _, buf.len()) };
        if result == -1 {
            Err(io::Error::last_os_error())
        }
        else {
            Ok(result as usize)
        }
    }

    pub fn write_async<'a>(&'a self, buf: &'a [u8]) ->
        impl Generator<Yield = YieldValue, Return = io::Result<usize>> + 'a
    {
        move || {
            handle_would_block!(self.write(buf))
        }
    }
}

impl Pipe {
    fn new() -> io::Result<(ReadPipe, WritePipe)> {
        let mut fds = [0, 0];
        if unsafe { pipe2(fds.as_mut_ptr(), O_NONBLOCK) } == 0 {
            Ok((ReadPipe::new(fds[0]), WritePipe::new(fds[1])))
        }
        else {
            Err(io::Error::last_os_error())
        }
    }
}

struct IntMap {
    data: Vec<Option<GenTask>>,
}

impl IntMap {
    fn new() -> Self {
        Self {
            data: vec![],
        }
    }

    fn take(&mut self, index: usize) -> Option<GenTask> {
        if index < self.data.len() {
            mem::replace(&mut self.data[index], None)
        }
        else {
            None
        }
    }

    fn insert(&mut self, index: usize, value: GenTask) {
        let len = self.data.len();
        if index >= len {
            let new_len = index + 1;
            unsafe { self.data.set_len(new_len) };
            for i in len..new_len {
                self.data[i] = None;
            }
        }
        self.data[index] = Some(value);
    }
}

pub struct EventLoop {
    events: [epoll_event; FD_COUNT],
    event_count: usize,
    fd_set: c_int,
    read_pipe: ReadPipe,
    tasks: IntMap,
}

impl EventLoop {
    pub fn new() -> io::Result<Self> {
        let (read_pipe, write_pipe) = Pipe::new()?;
        PIPE_WRITER_FD.with(|fd| {
            fd.set(write_pipe.as_raw_fd());
        });
        let fd_set = unsafe { epoll_create1(0) };
        let event_loop = Self {
            events: unsafe { mem::zeroed() },
            event_count: 0,
            fd_set,
            read_pipe,
            tasks: IntMap::new(),
        };
        event_loop.add_fd(event_loop.read_pipe.as_raw_fd());
        Ok(event_loop)
    }

    pub fn add_fd(&self, fd: RawFd) {
        let mut event = epoll_event {
            events: EPOLLIN as u32,
            u64: fd as u64,
        };
        unsafe { epoll_ctl(self.fd_set, EPOLL_CTL_ADD, fd, &mut event) };
    }

    fn add_task(&mut self, mut generator: GenTask) {
        loop {
            match unsafe { generator.resume() } {
                GeneratorState::Yielded(value) => {
                    match value {
                        Fd(fd) => {
                            self.add_fd(fd);
                            self.tasks.insert(fd as usize, generator);
                            break;
                        },
                        Task(generator) => self.add_task(generator),
                    }
                },
                GeneratorState::Complete(_) => break,
            }
        }
    }

    pub fn run<G: Generator<Yield=YieldValue>>(&mut self, mut generator: G) -> io::Result<G::Return> {
        loop {
            match unsafe { generator.resume() } {
                GeneratorState::Yielded(value) => {
                    match value {
                        Fd(fd) => self.add_fd(fd),
                        Task(generator) => self.add_task(generator),
                    }
                    self.wait()?;
                    for i in 0..self.event_count {
                        let current_fd = self.events[i].u64 as usize;
                        if let Some(mut task) = self.tasks.take(current_fd) {
                            match unsafe { task.resume() } {
                                GeneratorState::Yielded(value) => {
                                    match value {
                                        Fd(fd) => self.add_fd(fd),
                                        Task(generator) => self.add_task(generator),
                                    }
                                    self.tasks.insert(current_fd, task);
                                },
                                GeneratorState::Complete(_) => (),
                            }
                        }
                        else if current_fd as RawFd == self.read_pipe.as_raw_fd() {
                            self.add_fd(self.read_pipe.as_raw_fd());
                        }
                    }
                },
                GeneratorState::Complete(value) => return Ok(value),
            }
        }
    }

    pub fn wait(&mut self) -> io::Result<()> {
        let result = unsafe { epoll_wait(self.fd_set, self.events.as_ptr() as *mut _, FD_COUNT as c_int, -1) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            self.event_count = result as usize;
            Ok(())
        }
    }

    pub fn write_pipe() -> WritePipe {
        let fd = Cell::new(-1);
        PIPE_WRITER_FD.with(|writer_fd| {
            fd.set(writer_fd.get());
        });
        WritePipe {
            fd: fd.get() as RawFd,
        }
    }
}

/*pub fn accept_async<'a>(event_loop: &'a EventLoop, listener: &'a TcpListener) ->
    impl Generator<Yield = (), Return = io::Result<(TcpStream, SocketAddr)>> + 'a
{
    move || {
        loop {
            match listener.accept() {
                Err(error) => {
                    if error.kind() == WouldBlock {
                        event_loop.add_fd(listener.as_raw_fd());
                        yield;
                    }
                    else {
                        return Err(error);
                    }
                },
                Ok(result) => return Ok(result),
            }
        }
    }
}*/

pub struct TcpListener {
    listener: net::TcpListener,
}

impl Deref for TcpListener {
    type Target = net::TcpListener;

    fn deref(&self) -> &Self::Target {
        &self.listener
    }
}

impl DerefMut for TcpListener {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.listener
    }
}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let listener = net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
        })
    }

    async!(
    pub fn accept_async(&self) -> io::Result<(TcpStream, SocketAddr)> {
        handle_would_block!(self.accept(), |(stream, address)| {
            stream.set_nonblocking(true)?;
            (TcpStream { stream }, address)
        })
    }
    );
}

pub struct TcpStream {
    stream: net::TcpStream,
}

impl Deref for TcpStream {
    type Target = net::TcpStream;

    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

impl DerefMut for TcpStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}

impl TcpStream {
    // TODO: should connect be async?
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let stream = net::TcpStream::connect(addr)?;
        stream.set_nonblocking(true).expect("set nonblocking");
        Ok(Self {
            stream,
        })
    }

    pub fn write_async<'a>(&'a mut self, buf: &'a [u8]) ->
        impl Generator<Yield = YieldValue, Return = io::Result<usize>> + 'a
    {
        move || {
            handle_would_block!(self.write(buf))
        }
    }

    pub fn read_async<'a>(&'a mut self, buf: &'a mut [u8]) ->
        impl Generator<Yield = YieldValue, Return = io::Result<usize>> + 'a
    {
        move || {
            handle_would_block!(self.read(buf))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::{Generator, GeneratorState};
    use std::thread;

    use {
        EventLoop,
        TcpListener,
        TcpStream,
        YieldValue::Task,
    };

    #[test]
    fn tcp() {
        thread::spawn(|| {
            let mut event_loop = EventLoop::new().expect("event loop");
            event_loop.run(static || {
                let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
                let mut buffer = vec![0; 4096];
                await!(stream.read_async(&mut buffer))
                    .expect("read");
                println!("Read: {}", String::from_utf8_lossy(&buffer));
            })
                .expect("run event loop");
        });

        thread::spawn(|| {
            let mut event_loop = EventLoop::new().expect("event loop");
            event_loop.run(static || {
                let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
                let mut buffer = vec![0; 4096];
                await!(stream.read_async(&mut buffer))
                    .expect("read");
                println!("Read: {}", String::from_utf8_lossy(&buffer));
            })
                .expect("run event loop");
        });

        let mut event_loop = EventLoop::new().expect("event loop");
        let listener = TcpListener::bind("127.0.0.1:8080").expect("listener");
        /*let (client, _address) = {
            let mut generator = accept_async(&event_loop, &listener);
            let result;
            loop {
                match unsafe { generator.resume() } {
                    GeneratorState::Yielded(_) => { let _ = event_loop.wait(); },
                    GeneratorState::Complete(value) => {
                        result = value;
                        break;
                    },
                }
            }
            result
        }.expect("accept");*/
        event_loop.run(static || {
            for i in 0..2 {
                let (mut client, _address) = await!(listener.accept_async())
                    .expect("accept");
                /*yield Task(Box::new(static move || {
                    let msg = format!("hello {}", i);
                    await!(write_async(&mut client, msg.as_bytes()))
                        .expect("write");
                }));
                let write_pipe = EventLoop::write_pipe();
                await!(write_pipe.write_async(b"0"))
                    .expect("Write async");*/
                (spawn! {
                    let msg = format!("hello {}", i);
                    await!(client.write_async(msg.as_bytes()))
                        .expect("write");
                })
                    .expect("spawn");
            }
        })
            .expect("run event loop");
    }
}
