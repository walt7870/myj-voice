use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub spec: String,
    pub price: f64,
}

impl Product {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        aliases: Vec<&str>,
        spec: impl Into<String>,
        price: f64,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            aliases: aliases.into_iter().map(ToString::to_string).collect(),
            spec: spec.into(),
            price,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProductMatch {
    pub product_id: String,
    pub name: String,
    pub spec: String,
    pub quantity: u32,
    pub unit_price: f64,
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_goods_gid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_goods_no: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goods_gid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goods_no: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_product_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_sku_code: Option<String>,
}

pub fn match_products(text: &str, products: &[Product]) -> Vec<ProductMatch> {
    let mut matches = Vec::new();
    for product in products {
        let mut matched_name: Option<&str> = None;
        if text.contains(&product.name) {
            matched_name = Some(&product.name);
        } else {
            for alias in &product.aliases {
                if text.contains(alias) {
                    matched_name = Some(alias);
                    break;
                }
            }
        }
        let Some(name) = matched_name else {
            continue;
        };
        matches.push(ProductMatch {
            product_id: product.id.clone(),
            name: product.name.clone(),
            spec: product.spec.clone(),
            quantity: quantity_before(text, name).unwrap_or(1),
            unit_price: product.price,
            confidence: 0.86,
            parent_goods_gid: None,
            parent_goods_no: None,
            goods_gid: None,
            goods_no: None,
            mcp_product_id: None,
            mcp_sku_code: None,
        });
    }
    matches
}

fn quantity_before(text: &str, name: &str) -> Option<u32> {
    let index = text.find(name)?;
    let prefix = &text[..index];
    for (word, value) in [
        ("十", 10),
        ("九", 9),
        ("八", 8),
        ("七", 7),
        ("六", 6),
        ("五", 5),
        ("四", 4),
        ("三", 3),
        ("两", 2),
        ("二", 2),
        ("一", 1),
    ] {
        if prefix.ends_with(word)
            || prefix.ends_with(&format!("{word}瓶"))
            || prefix.ends_with(&format!("{word}个"))
        {
            return Some(value);
        }
        if prefix.ends_with(&format!("{word}杯")) || prefix.ends_with(&format!("{word}份")) {
            return Some(value);
        }
    }
    let digits = prefix
        .chars()
        .rev()
        .skip_while(|ch| matches!(ch, '瓶' | '个' | '杯' | '份' | ' ' | '，' | ','))
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        digits.chars().rev().collect::<String>().parse().ok()
    }
}
