import zmq
"""
A simple ZeroMQ server that listens for messages from clients and sends a response back.
"""

def main():
    # Create a ZeroMQ context
    context = zmq.Context()

    # Create a socket for the server
    socket = context.socket(zmq.REP)

    # Bind the socket to a specific address
    socket.bind("tcp://*:5555")

    print("Server started. Listening for messages...")

    while True:
        # Wait for a message from a client
        message = socket.recv()

        # Print the received message
        print(f"Received message: {message.decode()}")

        # Send a response back to the client
        response = "Message received"
        socket.send(response.encode())

if __name__ == "__main__":
    main()