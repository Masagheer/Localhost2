use std::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd}; 
use std::collections::HashMap;
use libc::{
    epoll_create1, epoll_ctl, epoll_wait, epoll_event,
    EPOLLIN, EPOLLERR, EPOLLHUP, EPOLL_CTL_ADD, EPOLL_CTL_DEL,
};
use std::time::{Instant, Duration};

const MAX_EVENTS: usize = 1024;
const TIMEOUT_MS: i32 = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30); // 30 seconds timeout

#[allow(dead_code)]
#[derive(Debug, PartialEq, Copy, Clone)]
enum StatusCode {
    Ok = 200,
    BadRequest = 400,
    Forbidden = 403,
    NotFound = 404,
    MethodNotAllowed = 405,
    PayloadTooLarge = 413,
    InternalServerError = 500,
}

impl StatusCode {
    fn to_string(&self) -> &str {
        match self {
            StatusCode::Ok => "200 OK",
            StatusCode::BadRequest => "400 Bad Request",
            StatusCode::Forbidden => "403 Forbidden",
            StatusCode::NotFound => "404 Not Found",
            StatusCode::MethodNotAllowed => "405 Method Not Allowed",
            StatusCode::PayloadTooLarge => "413 Payload Too Large",
            StatusCode::InternalServerError => "500 Internal Server Error",
        }
    }
}

struct HttpResponse {
    status: StatusCode,
    content_type: String,
    body: String,
}

// Add these new fields to track request state
struct Connection {
    stream: TcpStream,
    last_activity: Instant,
    buffer: Vec<u8>,
    chunked: bool,
    content_length: Option<usize>,
    request_complete: bool,
}

impl Connection {
    fn new(stream: TcpStream) -> Self {
        Connection {
            stream,
            last_activity: Instant::now(),
            buffer: Vec::new(),
            chunked: false,
            content_length: None,
            request_complete: false,
        }
    }
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
            // Check for timeouts before waiting for events
            self.check_timeouts()?;

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

                self.connections.insert(fd, Connection::new(stream));

                // self.connections.insert(fd, Connection { 
                //     stream,
                //     last_activity: Instant::now(),
                // });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("Error accepting connection: {}", e),
        }
        Ok(())
    }

    fn remove_connection(&mut self, fd: RawFd) -> io::Result<()> {
        unsafe {
            epoll_ctl(self.epoll_fd, EPOLL_CTL_DEL, fd, std::ptr::null_mut());
        }
        self.connections.remove(&fd);
        Ok(())
    }

    fn handle_client_data(&mut self, fd: RawFd) -> io::Result<()> {
        let mut temp_buffer = [0; 4096];
        
        if let Some(connection) = self.connections.get_mut(&fd) {
            connection.last_activity = Instant::now();

            match connection.stream.read(&mut temp_buffer) {
                Ok(0) => {
                    println!("Connection closed by client");
                    return Err(io::Error::new(io::ErrorKind::Other, "Connection closed"));
                }
                Ok(n) => {
                    connection.buffer.extend_from_slice(&temp_buffer[..n]);
                    
                    if !connection.request_complete {
                        if let Some((headers_end, headers)) = Self::parse_headers(&connection.buffer) {
                            // First time processing headers
                            if connection.content_length.is_none() && !connection.chunked {
                                // Check transfer encoding
                                if let Some(encoding) = headers.get("transfer-encoding") {
                                    connection.chunked = encoding.to_lowercase().contains("chunked");
                                }
                                
                                // Check content length
                                if let Some(length) = headers.get("content-length") {
                                    if let Ok(len) = length.parse::<usize>() {
                                        connection.content_length = Some(len);
                                    }
                                }
                            }

                            if connection.chunked {
                                connection.request_complete = Self::process_chunked_request(&connection.buffer[headers_end..]);
                            } else if let Some(content_length) = connection.content_length {
                                connection.request_complete = connection.buffer.len() >= headers_end + content_length;
                            } else {
                                // No body expected
                                connection.request_complete = true;
                            }
                        }
                    }

                    if connection.request_complete {
                        // Process the complete request
                        let response = Self::process_request(&connection.buffer)?;
                        Self::send_response(&mut connection.stream, response)?;
                        
                        // Reset connection state for next request
                        connection.buffer.clear();
                        connection.chunked = false;
                        connection.content_length = None;
                        connection.request_complete = false;
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
        Ok(())
    }

    fn parse_headers(buffer: &[u8]) -> Option<(usize, HashMap<String, String>)> {
        if let Ok(data) = String::from_utf8(buffer.to_vec()) {
            if let Some(headers_end) = data.find("\r\n\r\n") {
                let headers_str = &data[..headers_end];
                let mut headers = HashMap::new();
                
                for line in headers_str.split("\r\n").skip(1) {
                    if let Some((key, value)) = line.split_once(": ") {
                        headers.insert(key.to_lowercase(), value.to_string());
                    }
                }
                
                return Some((headers_end + 4, headers));
            }
        }
        None
    }

    fn process_chunked_request(data: &[u8]) -> bool {
        if let Ok(data_str) = String::from_utf8(data.to_vec()) {
            let mut pos = 0;
            let mut found_end = false;

            while pos < data_str.len() {
                // Find chunk size line
                if let Some(size_end) = data_str[pos..].find("\r\n") {
                    // Parse chunk size (hex)
                    if let Ok(chunk_size) = usize::from_str_radix(data_str[pos..pos+size_end].trim(), 16) {
                        if chunk_size == 0 {
                            found_end = true;
                            break;
                        }
                        
                        // Skip chunk data and CRLF
                        pos += size_end + 2 + chunk_size + 2;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            
            return found_end;
        }
        false
    }

    fn process_request(buffer: &[u8]) -> io::Result<HttpResponse> {
        // Process the complete request (either chunked or regular)
        if let Ok(request_str) = String::from_utf8(buffer.to_vec()) {
            let lines: Vec<&str> = request_str.split("\r\n").collect();
            if !lines.is_empty() {
                println!("Processing complete request");
                println!("Request line: {}", lines[0]);
            }
        }
        
        Ok(HttpResponse {
            status: StatusCode::Ok,
            content_type: "text/html; charset=utf-8".to_string(),
            body: "<html><body><h1>Hello from Rust Server!</h1><p>Your request was received.</p></body></html>".to_string(),
        })
    }
    
    #[allow(dead_code)]
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
    
    fn send_response(stream: &mut TcpStream, response: HttpResponse) -> io::Result<()> {
        let current_time = chrono::Utc::now();
        let date = current_time.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        
        let response_string = format!(
            "HTTP/1.1 {}\r\n\
            Server: RustServer/1.0\r\n\
            Date: {}\r\n\
            Content-Type: {}\r\n\
            Content-Length: {}\r\n\
            Connection: keep-alive\r\n\
            \r\n\
            {}",
            response.status.to_string(),
            date,
            response.content_type,
            response.body.len(),
            response.body
        );
        
        stream.write_all(response_string.as_bytes())?;
        stream.flush()?;
        Ok(())
    }

    #[allow(dead_code)]
    fn create_error_page(status: StatusCode) -> HttpResponse {
        let body = format!(
            "<html>\
            <head><title>Error {}</title></head>\
            <body>\
            <h1>{}</h1>\
            <p>{}</p>\
            </body>\
            </html>",
            status as i32,
            status.to_string(),
            match status {
                StatusCode::BadRequest => "The request could not be understood by the server.",
                StatusCode::Forbidden => "You don't have permission to access this resource.",
                StatusCode::NotFound => "The requested resource could not be found.",
                StatusCode::MethodNotAllowed => "The requested method is not allowed for this resource.",
                StatusCode::PayloadTooLarge => "The request payload is too large.",
                StatusCode::InternalServerError => "An internal server error occurred.",
                _ => "",
            }
        );

        HttpResponse {
            status,
            content_type: "text/html; charset=utf-8".to_string(),
            body,
        }
    }

    fn check_timeouts(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let mut timed_out_fds = Vec::new();

        // Collect FDs of timed out connections
        for (&fd, conn) in self.connections.iter() {
            if now.duration_since(conn.last_activity) > REQUEST_TIMEOUT {
                timed_out_fds.push(fd);
            }
        }

        // Remove timed out connections
        for fd in timed_out_fds {
            println!("Connection timed out, closing...");
            if let Some(conn) = self.connections.get_mut(&fd) {
                // Send timeout response before closing
                let response = HttpResponse {
                    status: StatusCode::InternalServerError,
                    content_type: "text/html; charset=utf-8".to_string(),
                    body: "<html><body><h1>408 Request Timeout</h1><p>The request has timed out.</p></body></html>".to_string(),
                };
                let _ = Self::send_response(&mut conn.stream, response);
            }
            self.remove_connection(fd)?;
        }

        Ok(())
    }
}

fn main() -> io::Result<()> {
    let ports = vec![8080, 8000, 8001]; // Multiple ports
    let mut server = Server::new(&ports)?;
    server.run()
}