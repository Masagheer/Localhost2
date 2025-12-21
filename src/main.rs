use std::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd}; 
use std::collections::HashMap;
use libc::{
    epoll_create1, epoll_ctl, epoll_wait, epoll_event,
    EPOLLIN, EPOLLERR, EPOLLHUP, EPOLL_CTL_ADD, EPOLL_CTL_DEL,
};

const MAX_EVENTS: usize = 1024;
const TIMEOUT_MS: i32 = 1000;

struct Connection {
    stream: TcpStream,
}

struct Server {
    listeners: Vec<TcpListener>,
    epoll_fd: RawFd,
    connections: HashMap<RawFd, Connection>,
}

impl Server {
    pub fn new(ports: &[u16]) -> io::Result<Server> {
        let mut listeners = Vec::new();
        
        // Create epoll instance
        let epoll_fd = unsafe { epoll_create1(0) };
        if epoll_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Create listeners for each port
        for &port in ports {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", port))?;
            listener.set_nonblocking(true)?;
            
            // Add listener to epoll
            let mut event = epoll_event {
                events: EPOLLIN as u32,
                u64: listener.as_raw_fd() as u64,
            };

            unsafe {
                if epoll_ctl(
                    epoll_fd,
                    EPOLL_CTL_ADD,
                    listener.as_raw_fd(),
                    &mut event as *mut epoll_event,
                ) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            
            println!("Server listening on http://localhost:{}/", port);
            listeners.push(listener);
        }
        
        Ok(Server {
            listeners,
            epoll_fd,
            connections: HashMap::new(),
        })
    }
    
    pub fn run(&mut self) -> io::Result<()> {
        let mut events = vec![epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
        
        loop {
            let num_events = unsafe {
                epoll_wait(
                    self.epoll_fd,
                    events.as_mut_ptr(),
                    MAX_EVENTS as i32,
                    TIMEOUT_MS,
                )
            };

            if num_events < 0 {
                return Err(io::Error::last_os_error());
            }

            for i in 0..num_events as usize {
                let fd = events[i].u64 as RawFd;

                // Check if this fd belongs to any of our listeners
                if self.listeners.iter().any(|l| l.as_raw_fd() == fd) {
                    self.accept_connection(fd)?;
                } else {
                    if events[i].events & (EPOLLERR as u32 | EPOLLHUP as u32) != 0 {
                        self.remove_connection(fd)?;
                        continue;
                    }

                    if events[i].events & EPOLLIN as u32 != 0 {
                        if let Err(_) = self.handle_client_data(fd) {
                            self.remove_connection(fd)?;
                        }
                    }
                }
            }
        }
    }

    fn accept_connection(&mut self, listener_fd: RawFd) -> io::Result<()> {
        // Find the correct listener
        let listener = self.listeners.iter()
            .find(|l| l.as_raw_fd() == listener_fd)
            .unwrap();

        match listener.accept() {
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
                        EPOLL_CTL_ADD,
                        fd,
                        &mut event as *mut epoll_event
                    ) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }

                self.connections.insert(fd, Connection { stream });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("Error accepting connection: {}", e),
        }
        Ok(())
    }

    // ... rest of the implementation remains the same ...
    fn remove_connection(&mut self, fd: RawFd) -> io::Result<()> {
        unsafe {
            epoll_ctl(self.epoll_fd, EPOLL_CTL_DEL, fd, std::ptr::null_mut());
        }
        self.connections.remove(&fd);
        Ok(())
    }

    fn handle_client_data(&mut self, fd: RawFd) -> io::Result<()> {
        let mut should_send_response = false;
        let mut should_close = false;

        if let Some(connection) = self.connections.get_mut(&fd) {
            let mut buffer = [0; 4096];
            match connection.stream.read(&mut buffer) {
                Ok(0) => {
                    println!("Connection closed by client");
                    return Err(io::Error::new(io::ErrorKind::Other, "Connection closed"));
                }
                Ok(n) => {
                    if let Some((request_line, headers)) = Self::parse_http_request(&buffer, n) {
                        println!("Request line: {}", request_line);
                        println!("Headers: {:?}", headers);

                        // Check if client wants to close connection
                        if let Some(connection_header) = headers.get("connection") {
                            should_close = connection_header.to_lowercase() == "close";
                        }

                        should_send_response = true;
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
        }

        if should_send_response {
            if let Some(conn) = self.connections.get_mut(&fd) {
                Self::send_response(&mut conn.stream)?;
            }
        }

        if should_close {
            return Err(io::Error::new(io::ErrorKind::Other, "Client requested close"));
        }

        Ok(())
    }

    fn parse_http_request(buffer: &[u8], size: usize) -> Option<(String, HashMap<String, String>)>{
        if let Ok(request_str) = String::from_utf8(buffer[..size].to_vec()){
            let lines: Vec<&str> = request_str.split("\r\n").collect();
            if lines.is_empty(){
                return None;
            }

            // Parse request line
            let request_line = lines[0];

            // Parse headers
            let mut headers = HashMap::new();
            for line in lines.iter().skip(1){
                if line.is_empty(){
                    break;
                }
                if let Some((key, value)) = line.split_once(": "){
                    headers.insert(key.to_lowercase(), value.to_string());
                };
            }

            return Some((request_line.to_string(), headers))
        }
        None
    }
    
    fn send_response(stream: &mut TcpStream) -> io::Result<()> {
        let current_time = chrono::Utc::now();
        let date = current_time.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let html = "<html><body><h1>Hello from Rust Server!</h1><p>Your request was received.</p></body></html>";
        let content_length = html.len();
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
            Server: RustServer/1.0\r\n\
            Date: {}\r\n\
            Content-Type: text/html; charset=utf-8\r\n\
            Content-Length: {}\r\n\
            Connection: keep-alive\r\n\
            \r\n\
            {}", date, content_length, html);
        
        stream.write_all(response.as_bytes())?;
        stream.flush()?;
        Ok(())
    }
}

fn main() -> io::Result<()> {
    let ports = vec![8080, 8000, 8001]; // Multiple ports
    let mut server = Server::new(&ports)?;
    server.run()
}