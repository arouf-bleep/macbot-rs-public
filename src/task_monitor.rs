use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures_util::FutureExt;

use crate::bot::state::TradingBot;

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&str>() {
        (*msg).to_string()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

pub fn spawn_monitored_task<F>(name: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let _task = tokio::spawn(async move {
        if let Err(payload) = AssertUnwindSafe(fut).catch_unwind().await {
            tracing::error!(
                "Background task '{name}' panicked: {}",
                panic_payload_message(payload.as_ref())
            );
        }
    });
}

pub fn spawn_critical_bot_task<F>(name: &'static str, bot: Arc<TradingBot>, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let _task = spawn_critical_bot_task_handle(name, bot, fut);
}

pub fn spawn_critical_bot_task_handle<F>(
    name: &'static str,
    bot: Arc<TradingBot>,
    fut: F,
) -> tokio::task::JoinHandle<Result<(), String>>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(payload) = AssertUnwindSafe(fut).catch_unwind().await {
            let message = panic_payload_message(payload.as_ref());
            tracing::error!("Critical background task '{name}' panicked: {message}");
            bot.set_degraded_no_trade(true);
            bot.halt_trading(
                "background_task_panic",
                format!("task={name} panic={message}"),
            )
            .await;
            bot.record_reconcile_error(
                "background_task_panic",
                format!("task={name} panic={message}"),
            )
            .await;
            Err(message)
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::panic_payload_message;

    #[test]
    fn panic_payload_message_handles_str_payloads() {
        assert_eq!(panic_payload_message(&"boom"), "boom");
    }

    #[test]
    fn panic_payload_message_handles_string_payloads() {
        assert_eq!(panic_payload_message(&"boom".to_string()), "boom");
    }
}
