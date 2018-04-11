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
use std::ops::Generator;
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
    ($event_loop:expr, $async_func:ident ( $($args:expr),* )) => {{
        let mut generator = $async_func(&$event_loop $(,$args)*);
        let result;
        loop {
            match unsafe { generator.resume() } {
                GeneratorState::Yielded(_) => { let _ = $event_loop.wait(); },
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
    (pub fn $func_name:ident ( $event_loop:ident, $($arg_names:ident : $(& $arg_types:ident)*),* ) -> $ret_typ:ty $block:block) => {
        pub fn $func_name<'a>($event_loop: &'a EventLoop, $($arg_names: $(&'a $arg_types)*),*) ->
            impl Generator<Yield = (), Return = $ret_typ> + 'a
        {
            move || {
                $block
            }
        }
    };
}

#[macro_export]
macro_rules! handle_would_block {
    ($event_loop:expr, $object:ident . $func_name:ident ($($args:expr),*)) => {
        loop {
            match $object.$func_name($($args),*) {
                Err(error) => {
                    if error.kind() == WouldBlock {
                        $event_loop.add_fd($object.as_raw_fd());
                        yield;
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
pub fn accept_async(event_loop, listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    handle_would_block!(event_loop, listener.accept())
}
);

pub fn write_async<'a>(event_loop: &'a EventLoop, stream: &'a mut TcpStream, buf: &'a [u8]) ->
    impl Generator<Yield = (), Return = io::Result<usize>> + 'a
{
    move || {
        handle_would_block!(event_loop, stream.write(buf))
    }
}

pub fn read_async<'a>(event_loop: &'a EventLoop, stream: &'a mut TcpStream, buf: &'a mut [u8]) ->
    impl Generator<Yield = (), Return = io::Result<usize>> + 'a
{
    move || {
        handle_would_block!(event_loop, stream.read(buf))
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
            // TODO: should connect be async?
            let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut buffer = vec![0; 4096];
            await!(event_loop, read_async(&mut stream, &mut buffer))
                .expect("read");
            println!("Read: {}", String::from_utf8_lossy(&buffer));
        });

        thread::spawn(|| {
            let event_loop = EventLoop::new();
            let mut stream = TcpStream::connect("127.0.0.1:8080").expect("stream");
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut buffer = vec![0; 4096];
            await!(event_loop, read_async(&mut stream, &mut buffer))
                .expect("read");
            println!("Read: {}", String::from_utf8_lossy(&buffer));
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
        for i in 0..2 {
            let (mut client, _address) = await!(event_loop, accept_async(&listener))
                .expect("accept");
            let msg = format!("hello {}", i);
            await!(event_loop, write_async(&mut client, msg.as_bytes()))
                .expect("write");
        }
    }
}
