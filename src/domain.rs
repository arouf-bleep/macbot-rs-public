use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    BuyYes,
    BuyNo,
    Unknown,
}

impl Direction {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "BUY_YES" | "YES" | "UP" => Self::BuyYes,
            "BUY_NO" | "NO" | "DOWN" => Self::BuyNo,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuyYes => "BUY_YES",
            Self::BuyNo => "BUY_NO",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeStatus {
    Open,
    Closing,
    Closed,
    Unknown,
}

impl TradeStatus {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "OPEN" => Self::Open,
            "CLOSING" => Self::Closing,
            "CLOSED" => Self::Closed,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Closing => "CLOSING",
            Self::Closed => "CLOSED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotMode {
    Live,
    Paper,
    Shadow,
    Simulation,
    Unknown,
}

impl BotMode {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "live" => Self::Live,
            "paper" => Self::Paper,
            "shadow" => Self::Shadow,
            "simulation" | "sim" => Self::Simulation,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Paper => "paper",
            Self::Shadow => "shadow",
            Self::Simulation => "simulation",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    Taker,
    Maker,
    Hybrid,
}

impl ExecutionMode {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "taker" => Self::Taker,
            "maker" => Self::Maker,
            _ => Self::Hybrid,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Taker => "taker",
            Self::Maker => "maker",
            Self::Hybrid => "hybrid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    BuyYes,
    BuyNo,
    Hold,
}

impl Signal {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "BUY_YES" => Self::BuyYes,
            "BUY_NO" => Self::BuyNo,
            _ => Self::Hold,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuyYes => "BUY_YES",
            Self::BuyNo => "BUY_NO",
            Self::Hold => "HOLD",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_handle_legacy_tokens() {
        assert_eq!(Direction::parse("BUY_YES"), Direction::BuyYes);
        assert_eq!(Direction::parse("down"), Direction::BuyNo);
        assert_eq!(Direction::parse("x"), Direction::Unknown);

        assert_eq!(TradeStatus::parse("OPEN"), TradeStatus::Open);
        assert_eq!(TradeStatus::parse("closed"), TradeStatus::Closed);
        assert_eq!(TradeStatus::parse("x"), TradeStatus::Unknown);

        assert_eq!(BotMode::parse("live"), BotMode::Live);
        assert_eq!(BotMode::parse("sim"), BotMode::Simulation);
        assert_eq!(BotMode::parse("x"), BotMode::Unknown);

        assert_eq!(ExecutionMode::parse("maker"), ExecutionMode::Maker);
        assert_eq!(ExecutionMode::parse("x"), ExecutionMode::Hybrid);

        assert_eq!(Signal::parse("BUY_NO"), Signal::BuyNo);
        assert_eq!(Signal::parse("noop"), Signal::Hold);
    }
}
