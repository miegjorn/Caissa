use std::collections::HashMap;
use regex::Regex;

// ─── Trait ───────────────────────────────────────────────────────────────────

pub trait PiiProxy: Send + Sync {
    /// Returns (redacted_text, vault) where vault maps placeholder → original.
    fn redact(&self, text: &str) -> (String, HashMap<String, String>);
}

// ─── RegexPiiProxy ───────────────────────────────────────────────────────────

pub struct RegexPiiProxy {
    patterns: Vec<(Regex, String)>, // (compiled pattern, placeholder prefix)
}

impl RegexPiiProxy {
    pub fn new(named_patterns: &[String]) -> anyhow::Result<Self> {
        let mut patterns = Vec::new();
        for name in named_patterns {
            let (re, prefix) = match name.as_str() {
                "email" => (
                    Regex::new(r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}")?,
                    "EMAIL",
                ),
                "phone" => (
                    Regex::new(r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b")?,
                    "PHONE",
                ),
                "ssn" => (
                    Regex::new(r"\b\d{3}-\d{2}-\d{4}\b")?,
                    "SSN",
                ),
                "credit_card" => (
                    Regex::new(r"\b(?:\d[ -]?){13,16}\b")?,
                    "CREDIT_CARD",
                ),
                _ => continue,
            };
            patterns.push((re, prefix.into()));
        }
        Ok(Self { patterns })
    }
}

impl PiiProxy for RegexPiiProxy {
    fn redact(&self, text: &str) -> (String, HashMap<String, String>) {
        let mut result = text.to_string();
        let mut vault: HashMap<String, String> = HashMap::new();
        let mut counter = 0u32;

        for (re, prefix) in &self.patterns {
            // Collect match spans before mutating the string.
            let matches: Vec<(usize, usize, String)> = re
                .find_iter(&result)
                .map(|m| (m.start(), m.end(), m.as_str().to_string()))
                .collect();

            // Replace in reverse order so earlier indices remain valid.
            for (start, end, original) in matches.into_iter().rev() {
                let placeholder = format!("[{}_{}]", prefix, counter);
                vault.insert(placeholder.clone(), original);
                result.replace_range(start..end, &placeholder);
                counter += 1;
            }
        }

        (result, vault)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email_address() {
        let proxy = RegexPiiProxy::new(&["email".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("contact me at user@example.com please");
        assert!(
            !redacted.contains("user@example.com"),
            "email should be redacted; got: {redacted}"
        );
        assert!(
            vault.values().any(|v| v == "user@example.com"),
            "vault should contain original email"
        );
    }

    #[test]
    fn redacts_phone_number() {
        let proxy = RegexPiiProxy::new(&["phone".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("call 555-123-4567 now");
        assert!(
            !redacted.contains("555-123-4567"),
            "phone should be redacted; got: {redacted}"
        );
        assert!(
            vault.values().any(|v| v == "555-123-4567"),
            "vault should contain original phone"
        );
    }

    #[test]
    fn redacts_ssn() {
        let proxy = RegexPiiProxy::new(&["ssn".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("my SSN is 123-45-6789.");
        assert!(!redacted.contains("123-45-6789"), "SSN should be redacted; got: {redacted}");
        assert!(vault.values().any(|v| v == "123-45-6789"));
    }

    #[test]
    fn no_false_positives_on_clean_text() {
        let proxy =
            RegexPiiProxy::new(&["email".to_string(), "phone".to_string()]).unwrap();
        let text = "This is a normal sentence with no PII.";
        let (redacted, vault) = proxy.redact(text);
        assert_eq!(redacted, text, "clean text should be unchanged");
        assert!(vault.is_empty(), "vault should be empty for clean text");
    }

    #[test]
    fn multiple_emails_in_one_string() {
        let proxy = RegexPiiProxy::new(&["email".to_string()]).unwrap();
        let (redacted, vault) =
            proxy.redact("alice@a.com and bob@b.com are contacts");
        assert!(!redacted.contains("alice@a.com"));
        assert!(!redacted.contains("bob@b.com"));
        assert_eq!(vault.len(), 2);
    }

    #[test]
    fn unknown_pattern_name_is_silently_skipped() {
        let proxy = RegexPiiProxy::new(&["nonexistent".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("hello world");
        assert_eq!(redacted, "hello world");
        assert!(vault.is_empty());
    }
}
