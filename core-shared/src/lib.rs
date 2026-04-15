use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<(String, String)>, // Price, Size (Strings to avoid precision issues in raw json)
    pub asks: Vec<(String, String)>,
}

#[allow(dead_code)]
impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
        }
    }

    // Helper to get best bid/ask as f64
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().and_then(|(p, _)| p.parse::<f64>().ok())
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().and_then(|(p, _)| p.parse::<f64>().ok())
    }

    pub fn is_valid(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => b < a,
            _ => true,
        }
    }
    
    // Update processes deltas properly
    pub fn update(&mut self, bids: Option<Vec<(String, String)>>, asks: Option<Vec<(String, String)>>) {
        if let Some(new_bids) = bids {
            for (price, size) in new_bids {

                if size == "0" {
                    self.bids.retain(|(p, _)| p != &price);
                } else if let Some(pos) = self.bids.iter().position(|(p, _)| p == &price) {
                    self.bids[pos] = (price, size);
                } else {
                    self.bids.push((price, size));
                }
            }
            // Sort bids descending
            self.bids.sort_by(|(p1, _), (p2, _)| {
                let p1_f = p1.parse::<f64>().unwrap_or(0.0);
                let p2_f = p2.parse::<f64>().unwrap_or(0.0);
                p2_f.partial_cmp(&p1_f).unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        if let Some(new_asks) = asks {
            for (price, size) in new_asks {
                if size == "0" {
                    self.asks.retain(|(p, _)| p != &price);
                } else if let Some(pos) = self.asks.iter().position(|(p, _)| p == &price) {
                    self.asks[pos] = (price, size);
                } else {
                    self.asks.push((price, size));
                }
            }
            // Sort asks ascending
            self.asks.sort_by(|(p1, _), (p2, _)| {
                let p1_f = p1.parse::<f64>().unwrap_or(1.0);
                let p2_f = p2.parse::<f64>().unwrap_or(1.0);
                p1_f.partial_cmp(&p2_f).unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MarketType {
    /// Crypto 5-minute candle (original HFT engine)
    Crypto5Min,
    /// Daily temperature prediction
    WeatherTemperature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Market {
    pub id: String,
    pub market_type: MarketType,
    pub orderbook: OrderBook,
    pub last_price: f64,
    pub expiration: i64,
    /// For crypto: strike price. For weather: not used.
    pub strike_price: f64,
    /// For weather markets: city name
    pub city: Option<String>,
    /// For weather markets: temperature outcome label
    pub outcome_label: Option<String>,
}

#[allow(dead_code)]
impl Market {
    pub fn new_weather(id: String, expiration: i64, city: String, label: String) -> Self {
        Self {
            id,
            market_type: MarketType::WeatherTemperature,
            orderbook: OrderBook::new(),
            last_price: 0.0,
            expiration,
            strike_price: 0.0,
            city: Some(city),
            outcome_label: Some(label),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Trade {
    pub price: f64,
    pub size: f64,
    pub side: String, // "BUY" or "SELL"
    pub timestamp: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orderbook_new_empty() {
        let ob = OrderBook::new();
        assert!(ob.bids.is_empty());
        assert!(ob.asks.is_empty());
        assert!(ob.best_bid().is_none());
        assert!(ob.best_ask().is_none());
    }

    #[test]
    fn test_orderbook_default() {
        let ob = OrderBook::default();
        assert!(ob.bids.is_empty());
        assert!(ob.asks.is_empty());
    }

    #[test]
    fn test_orderbook_best_bid_ask() {
        let mut ob = OrderBook::new();
        ob.update(
            Some(vec![
                ("0.50".to_string(), "10".to_string()),
                ("0.48".to_string(), "20".to_string()),
            ]),
            Some(vec![
                ("0.52".to_string(), "15".to_string()),
                ("0.55".to_string(), "25".to_string()),
            ]),
        );

        assert!((ob.best_bid().unwrap() - 0.50).abs() < 0.001);
        assert!((ob.best_ask().unwrap() - 0.52).abs() < 0.001);
    }

    #[test]
    fn test_orderbook_is_valid() {
        let mut ob = OrderBook::new();
        ob.update(
            Some(vec![("0.50".to_string(), "10".to_string())]),
            Some(vec![("0.52".to_string(), "10".to_string())]),
        );
        assert!(ob.is_valid()); // bid < ask

        // Empty is valid
        let ob2 = OrderBook::new();
        assert!(ob2.is_valid());
    }

    #[test]
    fn test_orderbook_update_remove_zero_size() {
        let mut ob = OrderBook::new();
        ob.update(
            Some(vec![("0.50".to_string(), "10".to_string())]),
            None,
        );
        assert_eq!(ob.bids.len(), 1);

        // Remove with size "0"
        ob.update(
            Some(vec![("0.50".to_string(), "0".to_string())]),
            None,
        );
        assert!(ob.bids.is_empty());
    }

    #[test]
    fn test_orderbook_update_replace_existing() {
        let mut ob = OrderBook::new();
        ob.update(
            Some(vec![("0.50".to_string(), "10".to_string())]),
            None,
        );
        assert_eq!(ob.bids[0].1, "10");

        // Update size
        ob.update(
            Some(vec![("0.50".to_string(), "25".to_string())]),
            None,
        );
        assert_eq!(ob.bids.len(), 1);
        assert_eq!(ob.bids[0].1, "25");
    }

    #[test]
    fn test_orderbook_bids_sorted_descending() {
        let mut ob = OrderBook::new();
        ob.update(
            Some(vec![
                ("0.40".to_string(), "10".to_string()),
                ("0.50".to_string(), "10".to_string()),
                ("0.45".to_string(), "10".to_string()),
            ]),
            None,
        );
        // Should be sorted descending: 0.50, 0.45, 0.40
        assert_eq!(ob.bids[0].0, "0.50");
        assert_eq!(ob.bids[1].0, "0.45");
        assert_eq!(ob.bids[2].0, "0.40");
    }

    #[test]
    fn test_orderbook_asks_sorted_ascending() {
        let mut ob = OrderBook::new();
        ob.update(
            None,
            Some(vec![
                ("0.55".to_string(), "10".to_string()),
                ("0.51".to_string(), "10".to_string()),
                ("0.53".to_string(), "10".to_string()),
            ]),
        );
        // Should be sorted ascending: 0.51, 0.53, 0.55
        assert_eq!(ob.asks[0].0, "0.51");
        assert_eq!(ob.asks[1].0, "0.53");
        assert_eq!(ob.asks[2].0, "0.55");
    }

    #[test]
    fn test_market_new_weather() {
        let market = Market::new_weather(
            "test-id".to_string(),
            1713200000,
            "Lucknow".to_string(),
            "45°C".to_string(),
        );
        assert_eq!(market.market_type, MarketType::WeatherTemperature);
        assert_eq!(market.city.as_deref(), Some("Lucknow"));
        assert_eq!(market.outcome_label.as_deref(), Some("45°C"));
        assert_eq!(market.strike_price, 0.0);
    }

    #[test]
    fn test_orderbook_serialization() {
        let mut ob = OrderBook::new();
        ob.update(
            Some(vec![("0.50".to_string(), "10".to_string())]),
            Some(vec![("0.52".to_string(), "5".to_string())]),
        );

        let json = serde_json::to_string(&ob).unwrap();
        let deserialized: OrderBook = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.bids.len(), 1);
        assert_eq!(deserialized.asks.len(), 1);
        assert!((deserialized.best_bid().unwrap() - 0.50).abs() < 0.001);
    }
}
