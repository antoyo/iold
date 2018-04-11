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
use std::os::raw::c_int;

const FD_COUNT: usize = 1024;

const EPOLL_CTL_ADD: c_int = 1;
const EPOLLIN: c_int = 0x1;

#[repr(C)]
struct epoll_event {
    pub events: u32,
    pub u64: u64,
}

extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
}

#[macro_export]
macro_rules! await {
    ($async_func:ident ( $($args:expr),* )) => {{
        let mut generator = $async_func($($args),*);
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
            impl Generator<Yield = RawFd, Return = $ret_typ> + 'a
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
                        yield $object.as_raw_fd();
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

pub struct EventLoop {
    events: [epoll_event; FD_COUNT],
    fd_set: c_int,
}

impl EventLoop {
    pub fn new() -> Self {
        let fd_set = unsafe { epoll_create1(0) };
        Self {
            events: unsafe { mem::uninitialized() },
            fd_set,
        }
    }

    pub fn add_fd(&self, fd: RawFd) {
        let mut event = epoll_event {
            events: EPOLLIN as u32,
            u64: fd as u64,
        };
        unsafe { epoll_ctl(self.fd_set, EPOLL_CTL_ADD, fd, &mut event) };
    }

    pub fn run<G: Generator<Yield=RawFd>>(&self, mut generator: G) -> G::Return {
        loop {
            match unsafe { generator.resume() } {
                GeneratorState::Yielded(fd) => self.add_fd(fd),
                GeneratorState::Complete(value) => return value,
            }
        }
    }

    pub fn wait(&self) -> Result<(), ()> {
        let result = unsafe { epoll_wait(self.fd_set, self.events.as_ptr() as *mut _, FD_COUNT as c_int, -1) };
        if result < 0 {
            Err(())
        } else {
            //self.event_count = result as usize;
            Ok(())
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
    impl Generator<Yield = RawFd, Return = io::Result<usize>> + 'a
{
    move || {
        handle_would_block!(stream.write(buf))
    }
}

pub fn read_async<'a>(stream: &'a mut TcpStream, buf: &'a mut [u8]) ->
    impl Generator<Yield = RawFd, Return = io::Result<usize>> + 'a
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

    use {EventLoop, accept_async, read_async, write_async};

    #[test]
    fn tcp() {
        thread::spawn(|| {
            let event_loop = EventLoop::new();
            event_loop.run(static || {
                // TODO: should connect be async?
                let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
                stream.set_nonblocking(true).expect("set nonblocking");
                let mut buffer = vec![0; 4096];
                await!(read_async(&mut stream, &mut buffer))
                    .expect("read");
                println!("Read: {}", String::from_utf8_lossy(&buffer));
            });
        });

        thread::spawn(|| {
            let event_loop = EventLoop::new();
            event_loop.run(static || {
                let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
                stream.set_nonblocking(true).expect("set nonblocking");
                let mut buffer = vec![0; 4096];
                await!(read_async(&mut stream, &mut buffer))
                    .expect("read");
                println!("Read: {}", String::from_utf8_lossy(&buffer));
            });
        });

        let event_loop = EventLoop::new();
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
                let msg = format!("hello {}", i);
                await!(write_async(&mut client, msg.as_bytes()))
                    .expect("write");
            }
        });
    }
}
