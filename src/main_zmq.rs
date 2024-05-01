use zmq::{Context, REQ};
use std::process::Command;

fn main() {
    // Get the command line argument
    let arg = std::env::args().nth(1).expect("Missing argument");

    // Create a ZeroMQ context
    let context = Context::new();

    // Create a socket of type REQ (request)
    let socket = context.socket(REQ).expect("Failed to create socket");

    // Set the socket timeout to 0.5 seconds
    socket.set_rcvtimeo(500).expect("Failed to set socket timeout");

    // Connect to the ZeroMQ server
    socket.connect("tcp://localhost:5555").expect("Failed to connect to server");

    // Send the argument to the server
    socket.send(arg.as_bytes(), 0).expect("Failed to send message");

//    // Receive the response from the server
//    let response = socket.recv_string(0).unwrap().expect("Failed to receive response");
//    println!("Response: {}", response);

    // Receive the response from the server
    match socket.recv_string(0) {
        Ok(response) => {
            let response = response.expect("Failed to receive response");
            println!("Response: {}", response);
        }
        Err(_) => {
            println!("Failed to receive response: Timeout");
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

            std::process::exit(0);
        }
    }

}