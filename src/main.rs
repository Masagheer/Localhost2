use std::net::{TcpListener};
use std::io::{Read, Write};

fn main(){
    // use TcpListener to bind address/port
    let listener = TcpListener::bind("127.0.0.1:8000").unwrap();
    println!("new connection on http://127.0.0.1:8000/");

    // incoming() is blocking so we need to fix it
    for mut stream in listener.incoming(){
        let mut stream = stream.unwrap();
        println!("new client connected");

        let mut buffer = [0; 1024];
        stream.read(&mut buffer).unwrap();

        println!("\n Request: {}", String::from_utf8_lossy(&buffer));

        let response = b"HTTP/1.1 200 OK\r\nContent-length: 5\r\n\r\nHello";
        stream.write_all(response).unwrap();

    }
}