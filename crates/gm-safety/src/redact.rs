use gm_core::types::{AxElement, AxSnapshot};

pub fn sanitize_ax_snapshot(mut snapshot: AxSnapshot) -> AxSnapshot {
    for el in &mut snapshot.elements {
        // Rule: Never read AXSecureTextField values
        if el.is_secure || el.role == "AXSecureTextField" {
            el.value = Some("[REDACTED_SECURE_FIELD]".into());
        }
    }
    snapshot
}

pub fn filter_secure_element(el: &mut AxElement) {
    if el.is_secure || el.role == "AXSecureTextField" {
        el.value = Some("[REDACTED]".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::types::Rect;

    #[test]
    fn test_secure_field_redaction() {
        let snap = AxSnapshot {
            window_id: 1,
            app_name: "Safari".into(),
            window_title: "Login".into(),
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            elements: vec![AxElement {
                token: "pwd_1".into(),
                role: "AXSecureTextField".into(),
                label: Some("Password".into()),
                value: Some("super_secret_123".into()),
                frame: Rect::new(10.0, 10.0, 50.0, 20.0),
                is_enabled: true,
                is_secure: true,
            }],
            timestamp_ms: 100,
        };

        let sanitized = sanitize_ax_snapshot(snap);
        assert_eq!(
            sanitized.elements[0].value.as_deref(),
            Some("[REDACTED_SECURE_FIELD]")
        );
    }
}
