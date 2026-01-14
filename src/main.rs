use std::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd}; 
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use libc::{
    epoll_create1, epoll_ctl, epoll_wait, epoll_event,
    EPOLLIN, EPOLLERR, EPOLLHUP, EPOLL_CTL_ADD, EPOLL_CTL_DEL,
};
use std::time::{Instant, Duration, SystemTime, UNIX_EPOCH};
use std::fs::{File, create_dir_all};
use std::path::Path;

const MAX_EVENTS: usize = 1024;
const TIMEOUT_MS: i32 = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30); // 30 seconds timeout
// Add these new structs and constants
const SESSION_TIMEOUT: Duration = Duration::from_secs(1800); // 30 minutes
const COOKIE_NAME: &str = "RUSTSESSIONID";

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
    headers: HashMap<String, String>,
}

#[derive(Debug, PartialEq)]
enum HttpMethod {
    GET,
    POST,
    DELETE,
    UNSUPPORTED,
}

#[allow(dead_code)]
struct HttpRequest {
    method: HttpMethod,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}


// Add these new fields to track request state
#[allow(dead_code)]
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

#[derive(Clone)]
#[allow(dead_code)]
struct Session {
    id: String,
    data: HashMap<String, String>,
    last_accessed: Instant,
}

impl Session {
    fn new(id: String) -> Self {
        Session {
            id,
            data: HashMap::new(),
            last_accessed: Instant::now(),
        }
    }

    fn update_access_time(&mut self) {
        self.last_accessed = Instant::now();
    }
}

struct SessionManager {
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
        }
    }

    fn create_session(&mut self) -> String {
        let mut hasher = DefaultHasher::new();
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .hash(&mut hasher);
        let session_id = format!("{:x}", hasher.finish());
        
        self.sessions.insert(session_id.clone(), Session::new(session_id.clone()));
        session_id
    }

    fn get_session(&mut self, session_id: &str) -> Option<&mut Session> {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.update_access_time();
            Some(session)
        } else {
            None
        }
    }

    fn cleanup_expired_sessions(&mut self) {
        let now = Instant::now();
        self.sessions.retain(|_, session| {
            now.duration_since(session.last_accessed) < SESSION_TIMEOUT
        });
    }
}

struct Server {
    listeners: Vec<TcpListener>,
    epoll_fd: RawFd,
    connections: HashMap<RawFd, Connection>,
    session_manager: SessionManager,
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
            session_manager: SessionManager::new(),
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

    fn parse_request(buffer: &[u8]) -> Option<HttpRequest> {
        // Find end of headers (\r\n\r\n) in the raw buffer to preserve binary body
        let sep = b"\r\n\r\n";
        if let Some(idx) = buffer.windows(sep.len()).position(|w| w == sep) {
            // Parse request line and headers as UTF-8 (headers are ASCII/UTF-8)
            if let Ok(head_str) = String::from_utf8(buffer[..idx].to_vec()) {
                let mut lines = head_str.split("\r\n");
                let request_line = lines.next()?;

                let request_parts: Vec<&str> = request_line.split_whitespace().collect();
                if request_parts.len() != 3 {
                    return None;
                }

                let method = match request_parts[0] {
                    "GET" => HttpMethod::GET,
                    "POST" => HttpMethod::POST,
                    "DELETE" => HttpMethod::DELETE,
                    _ => HttpMethod::UNSUPPORTED,
                };

                let path = request_parts[1].to_string();

                let mut headers = HashMap::new();
                for line in lines {
                    if let Some((k, v)) = line.split_once(": ") {
                        headers.insert(k.to_lowercase(), v.to_string());
                    }
                }

                // Body starts after the header separator
                let body = buffer[idx + sep.len()..].to_vec();

                return Some(HttpRequest { method, path, headers, body });
            }
        }
        None
    }

    fn handle_request(&mut self, request: HttpRequest) -> HttpResponse {
        // Parse cookies from request
        let cookies = Self::parse_cookies(&request.headers);
        let session_id = cookies.get(COOKIE_NAME);

        // Get or create session
        let session_id = match session_id {
            Some(id) => {
                if self.session_manager.get_session(id).is_some() {
                    id.clone()
                } else {
                    self.session_manager.create_session()
                }
            }
            None => self.session_manager.create_session(),
        };

        // Clean up expired sessions periodically
        self.session_manager.cleanup_expired_sessions();

        // Get session for request handling
        let session = self.session_manager.get_session(&session_id).unwrap();

        // Handle the request based on method
        let mut response = match request.method {
            HttpMethod::GET => Self::handle_get(&request, session),
            HttpMethod::POST => Self::handle_post(&request, session),
            HttpMethod::DELETE => Self::handle_delete(&request, session),
            HttpMethod::UNSUPPORTED => HttpResponse {
                status: StatusCode::MethodNotAllowed,
                content_type: "text/html; charset=utf-8".to_string(),
                body: "<html><body><h1>405 Method Not Allowed</h1></body></html>".to_string(),
                headers: HashMap::new(),
            },
        };

        // Add session cookie to response
        response.headers.insert(
            "Set-Cookie".to_string(),
            format!("{}={}; Path=/; HttpOnly", COOKIE_NAME, session_id)
        );

        response
    }

    fn parse_cookies(headers: &HashMap<String, String>) -> HashMap<String, String> {
        let mut cookies = HashMap::new();
        if let Some(cookie_header) = headers.get("cookie") {
            for cookie in cookie_header.split(';') {
                let cookie = cookie.trim();
                if let Some((key, value)) = cookie.split_once('=') {
                    cookies.insert(key.to_string(), value.to_string());
                }
            }
        }
        cookies
    }

    fn handle_get(request: &HttpRequest, session: &mut Session) -> HttpResponse {
        // Example of using session data
        let visit_count = session.data
            .entry("visit_count".to_string())
            .or_insert("0".to_string());
        let count = visit_count.parse::<i32>().unwrap_or(0) + 1;
        session.data.insert("visit_count".to_string(), count.to_string());

        // Simple router based on path
        match request.path.as_str() {
            "/" => {
                let body = format!(
                    "<html><body>\
                    <h1>Welcome to Rust Server!</h1>\
                    <p>You have visited this page {} times.</p>\
                    </body></html>",
                    count
                );

                HttpResponse {
                    status: StatusCode::Ok,
                    content_type: "text/html; charset=utf-8".to_string(),
                    body,
                    headers: HashMap::new(),
                }
            }
            "/about" => HttpResponse {
                status: StatusCode::Ok,
                content_type: "text/html; charset=utf-8".to_string(),
                body: "<html><body><h1>About Page</h1></body></html>".to_string(),
                headers: HashMap::new(),
            },
            "/upload" => {
                let body = "<html><body>\
                            <h1>Upload a file</h1>\
                            <form action=\"/upload\" method=\"post\" enctype=\"multipart/form-data\">\
                            <input type=\"file\" name=\"file\" />\
                            <input type=\"submit\" value=\"Upload\" />\
                            </form>\
                            </body></html>".to_string();

                HttpResponse {
                    status: StatusCode::Ok,
                    content_type: "text/html; charset=utf-8".to_string(),
                    body,
                    headers: HashMap::new(),
                }
            }
            p if p.starts_with("/uploads/") => {
                // Serve files saved in the uploads/ directory (text files like HTML/CSS/JS)
                let rel = &p["/uploads/".len()..];
                let path = Path::new("uploads").join(rel);

                if path.exists() && path.is_file() {
                    match std::fs::read_to_string(&path) {
                        Ok(contents) => {
                            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                            let content_type = match ext.as_str() {
                                "html" | "htm" => "text/html; charset=utf-8",
                                "css" => "text/css; charset=utf-8",
                                "js" => "application/javascript; charset=utf-8",
                                "json" => "application/json; charset=utf-8",
                                _ => "text/plain; charset=utf-8",
                            };

                            return HttpResponse {
                                status: StatusCode::Ok,
                                content_type: content_type.to_string(),
                                body: contents,
                                headers: HashMap::new(),
                            };
                        }
                        Err(_) => return Self::create_error_page(StatusCode::NotFound),
                    }
                } else {
                    return Self::create_error_page(StatusCode::NotFound);
                }
            }
            _ => HttpResponse {
                status: StatusCode::NotFound,
                content_type: "text/html; charset=utf-8".to_string(),
                body: "<html><body><h1>404 Not Found</h1></body></html>".to_string(),
                headers: HashMap::new(),
            },
        }
    }

    fn handle_post(request: &HttpRequest, _session: &mut Session) -> HttpResponse {
        // Handle POST request - support multipart/form-data file uploads
        if let Some(content_type) = request.headers.get("content-type") {
            if content_type.starts_with("multipart/form-data") {
                match Self::save_multipart_files(&request.body, content_type) {
                    Ok(files) => {
                        let mut body = String::from("<html><body><h1>Upload Successful</h1><ul>");
                        for f in files {
                            body.push_str(&format!("<li><a href=\"/uploads/{}\">{}</a></li>", f, f));
                        }
                        body.push_str("</ul></body></html>");

                        return HttpResponse {
                            status: StatusCode::Ok,
                            content_type: "text/html; charset=utf-8".to_string(),
                            body,
                            headers: HashMap::new(),
                        };
                    }
                    Err(e) => {
                        return HttpResponse {
                            status: StatusCode::InternalServerError,
                            content_type: "text/html; charset=utf-8".to_string(),
                            body: format!("<html><body><h1>500</h1><p>Error saving upload: {}</p></body></html>", e),
                            headers: HashMap::new(),
                        };
                    }
                }
            }
        }

        // Fallback: echo back the received data length
        let response_body = format!(
            "<html><body>\
            <h1>POST Request Received</h1>\
            <p>Path: {}</p>\
            <p>Received data length: {} bytes</p>\
            </body></html>",
            request.path,
            request.body.len()
        );

        HttpResponse {
            status: StatusCode::Ok,
            content_type: "text/html; charset=utf-8".to_string(),
            body: response_body,
            headers: HashMap::new(),
        }
    }

    fn save_multipart_files(body: &[u8], content_type: &str) -> io::Result<Vec<String>> {
        // Extract boundary
        let boundary = content_type
            .split(';')
            .find_map(|s| {
                let s = s.trim();
                if s.starts_with("boundary=") {
                    Some(s.trim_start_matches("boundary=").trim_matches('"').to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No boundary in content-type"))?;

        let marker = format!("--{}", boundary).into_bytes();
        let mut positions = Vec::new();
        let mut i = 0usize;
        while i + marker.len() <= body.len() {
            if &body[i..i + marker.len()] == marker.as_slice() {
                positions.push(i);
                i += marker.len();
            } else {
                i += 1;
            }
        }

        if positions.len() < 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "No multipart parts found"));
        }

        // Ensure uploads directory
        let upload_dir = Path::new("uploads");
        if !upload_dir.exists() {
            create_dir_all(upload_dir)?;
        }

        let mut saved = Vec::new();

        for part_index in 0..positions.len() - 1 {
            let start = positions[part_index] + marker.len();
            // Trim leading CRLF if present
            let mut part_start = start;
            if part_start + 2 <= body.len() && &body[part_start..part_start + 2] == b"\r\n" {
                part_start += 2;
            }
            let end = positions[part_index + 1];
            let mut part = &body[part_start..end];

            // If this is the final boundary with --, break
            if part.starts_with(b"--") {
                break;
            }

            // Find headers/body separator in part
            if let Some(hsep_pos) = part.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers_bytes = &part[..hsep_pos];
                let mut filename: Option<String> = None;

                if let Ok(headers_str) = String::from_utf8(headers_bytes.to_vec()) {
                    for line in headers_str.split("\r\n") {
                        if line.to_lowercase().starts_with("content-disposition:") {
                            // look for filename="..."
                            if let Some(idx) = line.find("filename=") {
                                let fname = line[idx + 9..].trim();
                                let fname = fname.trim_matches('"').trim_matches(' ');
                                if !fname.is_empty() {
                                    filename = Some(fname.to_string());
                                }
                            }
                        }
                    }
                }

                let mut file_data = part[hsep_pos + 4..].to_vec();
                // Trim trailing CRLF if present
                if file_data.ends_with(b"\r\n") {
                    file_data.truncate(file_data.len() - 2);
                }

                if let Some(fname) = filename {
                    let safe_name = fname.replace("..", "_");
                    let path = upload_dir.join(&safe_name);
                    let mut f = File::create(&path)?;
                    use std::io::Write as IoWrite;
                    f.write_all(&file_data)?;
                    saved.push(safe_name);
                }
            }
        }

        Ok(saved)
    }

    fn handle_delete(request: &HttpRequest, _session: &mut Session) -> HttpResponse {
        // Handle DELETE request
        // For now, just acknowledge the deletion request
        let response_body = format!(
            "<html><body>\
            <h1>DELETE Request Received</h1>\
            <p>Resource path: {}</p>\
            </body></html>",
            request.path
        );

        HttpResponse {
            status: StatusCode::Ok,
            content_type: "text/html; charset=utf-8".to_string(),
            body: response_body,
            headers: HashMap::new(),
        }
    }

    fn handle_client_data(&mut self, fd: RawFd) -> io::Result<()> {
        let mut response_to_send = None;
        
        if let Some(connection) = self.connections.get_mut(&fd) {
            connection.last_activity = Instant::now();

            let mut buffer = [0; 4096];
            match connection.stream.read(&mut buffer) {
                Ok(0) => {
                    println!("Connection closed by client");
                    return Err(io::Error::new(io::ErrorKind::Other, "Connection closed"));
                }
                Ok(n) => {
                    if let Some(request) = Self::parse_request(&buffer[..n]) {
                        println!("Received {:?} request for {}", request.method, request.path);
                        
                        response_to_send = Some(self.handle_request(request));
                    } else {
                        response_to_send = Some(HttpResponse {
                            status: StatusCode::BadRequest,
                            content_type: "text/html; charset=utf-8".to_string(),
                            body: "<html><body><h1>400 Bad Request</h1></body></html>".to_string(),
                            headers: HashMap::new(),
                        });
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
        
        // Send the response after releasing the mutable borrow
        if let Some(response) = response_to_send {
            if let Some(connection) = self.connections.get_mut(&fd) {
                Self::send_response(&mut connection.stream, response)?;
            }
        }
        
        Ok(())
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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
            headers: HashMap::new(),
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
        
        // Start with status line and basic headers
        let mut response_string = format!(
            "HTTP/1.1 {}\r\n\
            Server: RustServer/1.0\r\n\
            Date: {}\r\n\
            Content-Type: {}\r\n\
            Content-Length: {}\r\n",
            response.status.to_string(),
            date,
            response.content_type,
            response.body.len(),
        );

        // Add custom headers
        for (key, value) in response.headers {
            response_string.push_str(&format!("{}: {}\r\n", key, value));
        }

        // Add blank line and body
        response_string.push_str("\r\n");
        response_string.push_str(&response.body);
        
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
            headers: HashMap::new(),
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
                    headers: HashMap::new(),
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