# TcpConn
A wrapper for `std::net::TcpStream` which provides automatic data serialization
and deserialization. This simplifies communication by allowing the user to
represent messages as native structures and enums as long as they implement
[serde](https://serde.rs/) serialization traits.

## Security
The transmitted bytes are not encrypted

## Example
```rust
// declare custom message type
#[derive(Serialize, Deserialize)]
enum MyType {
    Ping,
    Greet{greeting: String}
}

// set up TCP stream
let stream = std::net::TcpStream::connect(...).unwrap();

// wrap TCP stream
let mut conn = TcpConn::new(stream).unwrap();

// define message
let msg = &MyType::Greet { greeting: "hello".to_string() };

// send message
conn.send::<MyType>(&msg).unwrap();

// receive message
let reply = conn.receive::<MyType>().unwrap();
```
