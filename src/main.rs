use std::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd}; 
// AsRawFd lets us extract the file descriptor (int) from TcpStream/TcpListener
// epoll works ONLY with file descriptors

use std::collections::HashMap;
use libc::{
    epoll_create1, // creates an epoll instance (kernel object)
    epoll_ctl,     // add/remove/modify fds in epoll
    epoll_wait,    // wait for events
    epoll_event,   // struct describing an event
    EPOLLIN,       // fd is ready to READ
    EPOLLERR,      // error occurred
    EPOLLHUP,      // hang up (client disconnected)
    EPOLL_CTL_ADD, // operation: add fd
    EPOLL_CTL_DEL, // operation: remove fd
};

const MAX_EVENTS: usize = 1024;
const TIMEOUT_MS: i32 = 1000; // 1 second timeout

struct Connection {
    stream: TcpStream,
}

struct Server {
    listener: TcpListener,
    port: u16,
    epoll_fd: RawFd,
    connections: HashMap<RawFd, Connection>,
}

impl Server {
    pub fn new(port: u16) -> io::Result<Server> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port))?;
        listener.set_nonblocking(true)?;
        
        // Create epoll instance
        let epoll_fd = unsafe { epoll_create1(0) };
        if epoll_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Add listener to epoll
        let mut event = epoll_event {
            events: EPOLLIN as u32, //tells you when fd is ready to read
            u64: listener.as_raw_fd() as u64,
        };

        unsafe {
        //the listener is officially registered in epoll.
            if epoll_ctl(
                epoll_fd,
                EPOLL_CTL_ADD, //add this fd
                listener.as_raw_fd(), //which fd
                &mut event as *mut epoll_event, //what events we care about
            ) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        
        println!("Server started on http://localhost:{}/", port);
        
        //return this as Result<Server>
        Ok(Server {
            listener,
            port,
            epoll_fd,
            connections: HashMap::new(),
        })
    }
    
    pub fn run(&mut self) -> io::Result<()> {
        let mut events = vec![epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
        
        loop {
            let num_events = unsafe {
                epoll_wait(
                    self.epoll_fd, //which epoll instance
                    events.as_mut_ptr(), //where to store events - inside the events vec! we created so it creates it as mutation to a pointer
                    MAX_EVENTS as i32, //max events to return
                    TIMEOUT_MS, //wait 1 second
                )
            };
						// if epoll_wait failed
            if num_events < 0 {
                return Err(io::Error::last_os_error());
            }
						//loop over the events
            for i in 0..num_events as usize {
		            //we stored the fd in events.u64 now we retrieve it to know which socket triggered the event
                let fd = events[i].u64 as RawFd;

                if fd == self.listener.as_raw_fd() {
		                //the listening socket is readable
		                //a new client wwants to connect
                    // Handle new connection
                    self.accept_connection()?;
                } else {
		                // This event belongs to an already connected client
                    // Handle existing connection
                    if events[i].events & (EPOLLERR as u32 | EPOLLHUP as u32) != 0 {
		                    // EPOLLERR → socket error
										    // EPOLLHUP → client disconnected
                        self.remove_connection(fd)?;
                        continue;
                    }

                    if events[i].events & EPOLLIN as u32 != 0 {
		                    // Kernel says: this socket has data to read
                        if let Err(_) = self.handle_client_data(fd) {
                            self.remove_connection(fd)?;
                        }
                    }
                }
            }
        }
    }

    fn accept_connection(&mut self) -> io::Result<()> {
        match self.listener.accept() {
            Ok((stream, addr)) => {
                println!("New connection from: {}", addr);
                stream.set_nonblocking(true)?;
                
                let fd = stream.as_raw_fd();
                let mut event = epoll_event {
                    events: EPOLLIN as u32,
                    u64: fd as u64,
                };

                unsafe {
                    if epoll_ctl(
	                    self.epoll_fd, 
	                    EPOLL_CTL_ADD, //Add client fd
	                    fd, 
	                    &mut event as *mut epoll_event
                    ) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }

                self.connections.insert(fd, Connection {
                    stream,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("Error accepting connection: {}", e),
        }
        Ok(())
    }

    fn remove_connection(&mut self, fd: RawFd) -> io::Result<()> {
        unsafe {
            epoll_ctl(
	            self.epoll_fd, 
		          EPOLL_CTL_DEL, //remove fd
		          fd, 
		          std::ptr::null_mut());
        }
        self.connections.remove(&fd);
        Ok(())
    }

    fn handle_client_data(&mut self, fd: RawFd) -> io::Result<()> {
        let should_send_response = if let Some(connection) = self.connections.get_mut(&fd) {
            let mut buffer = [0; 4096];
            match connection.stream.read(&mut buffer) {
                Ok(0) => {
                    // Connection closed by client
                    println!("Connection closed by client");
                    return Err(io::Error::new(io::ErrorKind::Other, "Connection closed"));
                }
                Ok(n) => {
                    if let Ok(request) = String::from_utf8(buffer[..n].to_vec()) {
                        println!("Received request:\n{}", request);
                        true
                    } else {
                        false
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("Error reading from client: {}", e);
                    return Err(e);
                }
            }
        } else {
            return Ok(());
        };

        if should_send_response {
            if let Some(conn) = self.connections.get_mut(&fd) {
                Self::send_response(&mut conn.stream, self.port)?;
            }
        }
        Ok(())
    }
    
    fn send_response(stream: &mut TcpStream, port: u16) -> io::Result<()> {
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/html\r\n\
             Content-Length: 98\r\n\
             \r\n\
             <html><body><h1>Hello from Rust Server on port {}!</h1><p>Your request was received.</p></body></html>",
            port
        );
        
        stream.write_all(response.as_bytes())?;
        stream.flush()?;
        Ok(())
    }
}

fn main() -> io::Result<()> {
    let mut server = Server::new(8080)?;
    server.run()
}