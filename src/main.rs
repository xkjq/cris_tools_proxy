use nng::{Protocol, Socket, Error};
use std::process::Command;
use rfd::MessageDialog;

fn main() {
    // Get the command line argument
    let arg = std::env::args().nth(1).expect("Missing argument");

    // Create a socket of type REQ (request)
    let socket = Socket::new(Protocol::Req0).expect("Failed to create socket");

    // Connect to the NNG server
    match socket.dial("tcp://localhost:5555") {
        Ok(_) => {
            println!("Connected to server");
        }
        Err(e) => {
            println!("Failed to connect to server: {:?}", e);
            let choice = MessageDialog::new()
                .set_title("CRIS Tools not running")
                .set_description("For advanced voice commands to function CRIS Tools needs to be running.\nDo you want to launch it?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();

            match choice {
                rfd::MessageDialogResult::Yes => {
                    println!("User chose Yes");
                    let output = Command::new(r"T:\rad-tools\cris_tools.exe")
                        .spawn();

                    match output {
                        Ok(_) => {
                            println!("CRIS Tools launched successfully");
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // Handle the NotFound error here
                            println!("CRIS Tools not found");
                            eprintln!("Failed to launch CRIS Tools: {:?}", e);
                        }
                        Err(e) => {
                            eprintln!("Failed to launch CRIS Tools: {:?}", e);
                        }
                    }
                },
                rfd::MessageDialogResult::No => println!("User chose No"),
                _ => println!("User closed or cancelled the dialog box"),
            }

            std::process::exit(1);
        }
    }

    // Send the argument to the server
    let message = "run/".to_string() + &arg;
    socket.send(message.as_bytes()).expect("Failed to send message");

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