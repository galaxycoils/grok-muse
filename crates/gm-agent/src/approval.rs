use crate::wheel::WheelHandoffRequest;
use async_trait::async_trait;
use gm_core::error::Result;
use gm_core::types::ComputerAction;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub task_id: String,
    pub step: u32,
    pub action: ComputerAction,
    pub target_label: Option<String>,
    pub reason: String,
}

#[async_trait]
pub trait InteractionHandler: Send + Sync {
    async fn request_approval(&self, req: &ApprovalRequest) -> Result<bool>;
    async fn ask_user(&self, question: &str) -> Result<Option<String>>;
    async fn request_wheel_handoff(&self, _req: &WheelHandoffRequest) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Default non-interactive handler that denies irreversible actions and returns None for ask_user.
#[derive(Debug, Clone, Default)]
pub struct NonInteractiveHandler;

#[async_trait]
impl InteractionHandler for NonInteractiveHandler {
    async fn request_approval(&self, _req: &ApprovalRequest) -> Result<bool> {
        Ok(false)
    }

    async fn ask_user(&self, _question: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

pub type ApprovalCallback = Arc<dyn Fn(&ApprovalRequest) -> bool + Send + Sync>;
pub type AskUserCallback = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type WheelHandoffCallback = Arc<dyn Fn(&WheelHandoffRequest) -> Option<String> + Send + Sync>;

/// Callback-based interaction handler for testing and CLI TTY interactions.
pub struct CallbackInteractionHandler {
    approval_cb: Option<ApprovalCallback>,
    ask_cb: Option<AskUserCallback>,
    wheel_cb: Option<WheelHandoffCallback>,
    approval_log: Arc<Mutex<Vec<ApprovalRequest>>>,
    wheel_log: Arc<Mutex<Vec<WheelHandoffRequest>>>,
}

impl Default for CallbackInteractionHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl CallbackInteractionHandler {
    pub fn new() -> Self {
        Self {
            approval_cb: None,
            ask_cb: None,
            wheel_cb: None,
            approval_log: Arc::new(Mutex::new(Vec::new())),
            wheel_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_approval_always(approved: bool) -> Self {
        Self {
            approval_cb: Some(Arc::new(move |_| approved)),
            ask_cb: None,
            wheel_cb: None,
            approval_log: Arc::new(Mutex::new(Vec::new())),
            wheel_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_approval_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&ApprovalRequest) -> bool + Send + Sync + 'static,
    {
        self.approval_cb = Some(Arc::new(cb));
        self
    }

    pub fn with_ask_user_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        self.ask_cb = Some(Arc::new(cb));
        self
    }

    pub fn with_wheel_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&WheelHandoffRequest) -> Option<String> + Send + Sync + 'static,
    {
        self.wheel_cb = Some(Arc::new(cb));
        self
    }

    pub fn requested_approvals(&self) -> Vec<ApprovalRequest> {
        self.approval_log.lock().clone()
    }

    pub fn requested_wheel_handoffs(&self) -> Vec<WheelHandoffRequest> {
        self.wheel_log.lock().clone()
    }
}

#[async_trait]
impl InteractionHandler for CallbackInteractionHandler {
    async fn request_approval(&self, req: &ApprovalRequest) -> Result<bool> {
        self.approval_log.lock().push(req.clone());
        if let Some(ref cb) = self.approval_cb {
            Ok(cb(req))
        } else {
            Ok(false)
        }
    }

    async fn ask_user(&self, question: &str) -> Result<Option<String>> {
        if let Some(ref cb) = self.ask_cb {
            Ok(cb(question))
        } else {
            Ok(None)
        }
    }

    async fn request_wheel_handoff(&self, req: &WheelHandoffRequest) -> Result<Option<String>> {
        self.wheel_log.lock().push(req.clone());
        if let Some(ref cb) = self.wheel_cb {
            Ok(cb(req))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_non_interactive_handler() {
        let handler = NonInteractiveHandler;
        let req = ApprovalRequest {
            task_id: "t1".into(),
            step: 1,
            action: ComputerAction::Done {
                summary: "done".into(),
            },
            target_label: None,
            reason: "test".into(),
        };
        assert!(!handler.request_approval(&req).await.unwrap());
        assert_eq!(handler.ask_user("question").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_callback_interaction_handler() {
        let handler = CallbackInteractionHandler::with_approval_always(true)
            .with_ask_user_callback(|q| Some(format!("Answer to: {q}")));

        let req = ApprovalRequest {
            task_id: "t2".into(),
            step: 2,
            action: ComputerAction::Click {
                token: Some("btn".into()),
                coords: None,
                button: None,
                click_count: None,
            },
            target_label: Some("Pay Now".into()),
            reason: "Payment requires confirmation".into(),
        };

        assert!(handler.request_approval(&req).await.unwrap());
        assert_eq!(handler.requested_approvals().len(), 1);
        assert_eq!(
            handler.ask_user("What card?").await.unwrap(),
            Some("Answer to: What card?".into())
        );
    }

    #[tokio::test]
    async fn test_wheel_handoff_callback() {
        let handler = CallbackInteractionHandler::new()
            .with_wheel_callback(|req| Some(format!("Solved '{}' manually", req.blocker)));
        let wheel_req = crate::wheel::WheelHandoffRequest {
            task_id: "t-wheel-1".into(),
            bot_id: Some("muse".into()),
            step: 3,
            blocker: "Cloudflare Turnstile CAPTCHA".into(),
            instructions: "Complete CAPTCHA and click Hand Back to Bot".into(),
        };
        let note = handler.request_wheel_handoff(&wheel_req).await.unwrap();
        assert_eq!(
            note.as_deref(),
            Some("Solved 'Cloudflare Turnstile CAPTCHA' manually")
        );
    }
}
