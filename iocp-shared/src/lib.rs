use serde::{Deserialize, Serialize};


#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    A,
    B,
    C
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    One,
    Two,
    Three
}
