//! Passenger types — a fare input (discounts are legally mandated for
//! seniors/PWDs in some markets), matching the passengers table CHECK.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassengerType {
    Adult,
    Child,
    Senior,
    Pwd,
    Infant,
}

impl PassengerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PassengerType::Adult => "adult",
            PassengerType::Child => "child",
            PassengerType::Senior => "senior",
            PassengerType::Pwd => "pwd",
            PassengerType::Infant => "infant",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "adult" => PassengerType::Adult,
            "child" => PassengerType::Child,
            "senior" => PassengerType::Senior,
            "pwd" => PassengerType::Pwd,
            "infant" => PassengerType::Infant,
            _ => return None,
        })
    }
}
