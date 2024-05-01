import pynng

def main():
    # Create a rep socket
    with pynng.Rep0(listen='tcp://127.0.0.1:5555') as socket:

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