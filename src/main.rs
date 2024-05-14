use nng::{Protocol, Socket, Error};
use std::process::Command;

fn main() {
    // Get the command line argument
    let arg = std::env::args().nth(1).expect("Missing argument");

    // Create a socket of type REQ (request)
    let socket = Socket::new(Protocol::Req0).expect("Failed to create socket");

    // Connect to the NNG server
    //socket.dial("tcp://localhost:5555").expect("Failed to connect to server");
    match socket.dial("tcp://localhost:5555") {
        Ok(_) => {
            println!("Connected to server");
        }
        Err(e) => {
            println!("Failed to connect to server: {:?}", e);
            let output = Command::new("ls")
                .arg("-l")
                .output()
                .expect("Failed to execute command");
            if output.status.success() {
                let s = String::from_utf8_lossy(&output.stdout);
                println!("Command output: {}", s);
            } else {
                let s = String::from_utf8_lossy(&output.stderr);
                println!("Command failed, stderr: {}", s);
            }
            std::process::exit(1);
        }
    }

    // Send the argument to the server
    socket.send(arg.as_bytes()).expect("Failed to send message");

    // Receive the response from the server
    match socket.recv() {
        Ok(response) => {
            let response = String::from_utf8(response.to_vec()).expect("Failed to receive response");
            println!("Response: {}", response);
        }
        Err(Error::TimedOut) => {
            println!("Failed to receive response: Timeout");
        }
        Err(e) => {
            println!("Failed to receive response: {:?}", e);
        }
    }
}