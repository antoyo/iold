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
 * An async function is simply a function that returns a generator (a macro could hide that).
 * Await (could be a macro) is yield and get the value from the event loop after the yield.
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
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::{Generator, GeneratorState};
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
    (pub fn $func_name:ident ( $($arg_names:ident : $(& $arg_types:ident)*),* ) -> $ret_typ:ty $block:block) => {
        pub fn $func_name<'a>($($arg_names: $(&'a $arg_types)*),*) ->
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
    ($object:ident . $func_name:ident ($($args:expr),*)) => {
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
                Ok(result) => return Ok(result),
            }
        }
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

    fn add_task(&mut self, mut generator: Box<Generator<Yield = YieldValue, Return = ()>>) {
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

async!(
pub fn accept_async(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    handle_would_block!(listener.accept())
}
);

pub fn write_async<'a>(stream: &'a mut TcpStream, buf: &'a [u8]) ->
    impl Generator<Yield = YieldValue, Return = io::Result<usize>> + 'a
{
    move || {
        handle_would_block!(stream.write(buf))
    }
}

pub fn read_async<'a>(stream: &'a mut TcpStream, buf: &'a mut [u8]) ->
    impl Generator<Yield = YieldValue, Return = io::Result<usize>> + 'a
{
    move || {
        handle_would_block!(stream.read(buf))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpStream, TcpListener};
    use std::ops::{Generator, GeneratorState};
    use std::thread;

    use {
        EventLoop,
        accept_async,
        read_async,
        write_async,
        YieldValue::Task,
    };

    #[test]
    fn tcp() {
        thread::spawn(|| {
            let mut event_loop = EventLoop::new().expect("event loop");
            event_loop.run(static || {
                // TODO: should connect be async?
                let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
                stream.set_nonblocking(true).expect("set nonblocking");
                let mut buffer = vec![0; 4096];
                await!(read_async(&mut stream, &mut buffer))
                    .expect("read");
                println!("Read: {}", String::from_utf8_lossy(&buffer));
            })
                .expect("run event loop");
        });

        thread::spawn(|| {
            let mut event_loop = EventLoop::new().expect("event loop");
            event_loop.run(static || {
                let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
                stream.set_nonblocking(true).expect("set nonblocking");
                let mut buffer = vec![0; 4096];
                await!(read_async(&mut stream, &mut buffer))
                    .expect("read");
                println!("Read: {}", String::from_utf8_lossy(&buffer));
            })
                .expect("run event loop");
        });

        let mut event_loop = EventLoop::new().expect("event loop");
        let listener = TcpListener::bind("127.0.0.1:8080").expect("listener");
        listener.set_nonblocking(true).expect("set nonblocking");
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
                let (mut client, _address) = await!(accept_async(&listener))
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
                    await!(write_async(&mut client, msg.as_bytes()))
                        .expect("write");
                })
                    .expect("spawn");
            }
        })
            .expect("run event loop");
    }
}
