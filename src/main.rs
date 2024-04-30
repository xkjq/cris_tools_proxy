use zmq::{Context, REQ};

fn main() {
    // Get the command line argument
    let arg = std::env::args().nth(1).expect("Missing argument");

    // Create a ZeroMQ context
    let context = Context::new();

    // Create a socket of type REQ (request)
    let socket = context.socket(REQ).expect("Failed to create socket");

    // Connect to the ZeroMQ server
    socket.connect("tcp://localhost:5555").expect("Failed to connect to server");

    // Send the argument to the server
    socket.send(arg.as_bytes(), 0).expect("Failed to send message");

    // Receive the response from the server
    let response = socket.recv_string(0).unwrap().expect("Failed to receive response");
    println!("Response: {}", response);
}
