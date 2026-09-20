/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `SKPaymentQueue` — StoreKit in-app purchase queue.
//!
//! With IAP emulation enabled (Cheat Engine `IAP` toggle or the
//! `TOUCHHLE_IAP_EMULATION` environment variable), `addPayment:` completes the
//! transaction locally as `SKPaymentTransactionStatePurchased`, so games that
//! gate content behind StoreKit unlock it without any App Store contact.
//! This only affects the emulated app inside touchHLE: no App Store, receipts
//! or Apple servers are involved, and nothing outside this process changes.
//!
//! With emulation disabled, `addPayment:` delivers a well-formed
//! `SKPaymentTransactionStateFailed` transaction so well-behaved games can
//! show their "purchases are disabled" path instead of hanging.

use crate::frameworks::foundation::{ns_string, NSInteger};
use crate::frameworks::store_kit::{
    emulation_enabled, SK_PAYMENT_TRANSACTION_STATE_FAILED, SK_PAYMENT_TRANSACTION_STATE_PURCHASED,
    SK_PAYMENT_TRANSACTION_STATE_RESTORED,
};
use crate::objc::{autorelease, id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, HostObject, NSZonePtr};
use crate::Environment;

// MARK: - Per-process state

/// Singleton cache and purchase history for `[SKPaymentQueue defaultQueue]`.
#[derive(Default)]
pub struct State {
    default_queue: Option<id>,
    transaction_counter: u64,
    /// Product identifiers "bought" this session; used by
    /// `restoreCompletedTransactions`.
    purchased: Vec<String>,
}

impl State {
    fn get(env: &mut Environment) -> &mut State {
        &mut env.framework_state.store_kit.payment_queue
    }
}

#[derive(Default)]
struct SKPaymentQueueHostObject {
    /// SKPaymentTransactionObserver — weak reference
    observer: id,
    /// Transactions delivered but not yet finished. Each is retained.
    pending: Vec<id>,
}
impl HostObject for SKPaymentQueueHostObject {}

/// Generate a fresh transaction identifier for emulated purchases.
fn next_transaction_identifier(env: &mut Environment, prefix: &str) -> id {
    let number = {
        let state = State::get(env);
        state.transaction_counter += 1;
        state.transaction_counter
    };
    ns_string::from_rust_string(env, format!("touchHLE.{prefix}.{number}"))
}

/// Read a guest NSString as an owned Rust string (empty if nil).
fn identifier_string(env: &mut Environment, string: id) -> String {
    if string == nil {
        return String::new();
    }
    ns_string::to_rust_string(env, string).to_owned()
}

/// Fields of an `SKPaymentTransaction`. Retained on assignment.
#[derive(Default)]
struct SKPaymentTransactionHostObject {
    state: NSInteger,
    transaction_identifier: id,
    payment: id,
    original_transaction: id,
}
impl HostObject for SKPaymentTransactionHostObject {}

/// Create a transaction. Returns it at the `alloc` refcount of 1; the caller
/// owns that reference and must balance it (autorelease for transient use, or
/// a pending-queue reference).
fn make_transaction(
    env: &mut Environment,
    state: NSInteger,
    payment: id,
    transaction_identifier: id,
    original_transaction: id,
) -> id {
    let transaction: id = msg_class![env; SKPaymentTransaction alloc];
    {
        let host = env.objc.borrow_mut::<SKPaymentTransactionHostObject>(transaction);
        host.state = state;
        host.transaction_identifier = transaction_identifier;
        host.payment = payment;
        host.original_transaction = original_transaction;
    }
    retain(env, transaction_identifier);
    if payment != nil {
        retain(env, payment);
    }
    if original_transaction != nil {
        retain(env, original_transaction);
    }
    transaction
}

/// Deliver `transactions` to the queue's observer via
/// `paymentQueue:updatedTransactions:`.
fn deliver_transactions(env: &mut Environment, queue: id, transactions: &[id]) {
    let observer = env.objc.borrow::<SKPaymentQueueHostObject>(queue).observer;
    if observer == nil || transactions.is_empty() {
        return;
    }
    let array: id = msg_class![env; NSMutableArray array];
    for &transaction in transactions {
        () = msg![env; array addObject:transaction];
    }
    let sel = env.objc.register_host_selector(
        "paymentQueue:updatedTransactions:".to_string(),
        &mut env.mem,
    );
    let responds: bool = msg![env; observer respondsToSelector:sel];
    if responds {
        () = msg![env; observer paymentQueue:queue updatedTransactions:array];
    } else {
        log!(
            "Warning: SKPaymentTransactionObserver does not respond to paymentQueue:updatedTransactions:; transaction result dropped."
        );
    }
}

/// Remember a purchased identifier for later restore calls.
fn remember_purchase(env: &mut Environment, identifier: &str) {
    if identifier.is_empty() {
        return;
    }
    let state = State::get(env);
    if !state.purchased.iter().any(|seen| seen == identifier) {
        state.purchased.push(identifier.to_owned());
    }
}

/// Create an autoreleased SKPayment for a Rust product identifier.
fn payment_for_identifier(env: &mut Environment, identifier: &str) -> id {
    let identifier = ns_string::from_rust_string(env, identifier.to_owned());
    autorelease(env, identifier);
    let payment: id = msg_class![env; SKPayment alloc];
    let payment: id = msg![env; payment initWithProductIdentifier:identifier];
    autorelease(env, payment)
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation SKPaymentQueue: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(SKPaymentQueueHostObject {
        observer: nil,
        pending: Vec::new(),
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

// MARK: - Singleton

+ (id)defaultQueue {
    // Always return the same singleton so observers are not lost between calls.
    if let Some(queue) = State::get(env).default_queue {
        return queue;
    }
    let queue: id = msg![env; this alloc];
    let queue: id = msg![env; queue init];
    // refcount=1 is our singleton retain — do NOT autorelease
    State::get(env).default_queue = Some(queue);
    log!("SKPaymentQueue defaultQueue: singleton created");
    queue
}

+ (bool)canMakePayments {
    emulation_enabled()
}

// MARK: - Init

- (id)init {
    this
}

- (())dealloc {
    let (observer, pending) = {
        let host = env.objc.borrow::<SKPaymentQueueHostObject>(this);
        (host.observer, host.pending.clone())
    };
    release(env, observer);
    for transaction in pending {
        release(env, transaction);
    }
    env.objc.dealloc_object(this, &mut env.mem)
}

// MARK: - Observers

- (())addTransactionObserver:(id)observer {
    // Убрали .unwrap()
    let host_obj = env.objc.borrow_mut::<SKPaymentQueueHostObject>(this);
    host_obj.observer = observer;
}

- (())removeTransactionObserver:(id)_observer {
    // Убрали .unwrap()
    let host_obj = env.objc.borrow_mut::<SKPaymentQueueHostObject>(this);
    host_obj.observer = nil;
}

// MARK: - Payment requests

- (())addPayment:(id)payment { // SKPayment*
    let identifier_object: id = if payment != nil {
        msg![env; payment productIdentifier]
    } else {
        nil
    };
    let identifier = identifier_string(env, identifier_object);

    if !emulation_enabled() {
        log!(
            "SKPaymentQueue addPayment: IAP emulation off; failing transaction for {:?}.",
            identifier
        );
        let error_identifier = ns_string::from_rust_string(env, identifier);
        autorelease(env, error_identifier);
        let transaction = make_transaction(
            env,
            SK_PAYMENT_TRANSACTION_STATE_FAILED,
            payment,
            error_identifier,
            nil,
        );
        autorelease(env, transaction);
        deliver_transactions(env, this, &[transaction]);
        return;
    }

    remember_purchase(env, &identifier);
    let transaction_identifier = next_transaction_identifier(env, "iap");
    let transaction = make_transaction(
        env,
        SK_PAYMENT_TRANSACTION_STATE_PURCHASED,
        payment,
        transaction_identifier,
        nil,
    );
    // The pending queue owns one reference; the pool guards the delivery call.
    retain(env, transaction);
    autorelease(env, transaction);
    env.objc
        .borrow_mut::<SKPaymentQueueHostObject>(this)
        .pending
        .push(transaction);
    let count = State::get(env).transaction_counter;
    log!(
        "SKPaymentQueue addPayment: IAP emulation ON; product {:?} auto-purchased (transaction #{}).",
        identifier,
        count
    );
    deliver_transactions(env, this, &[transaction]);
}

- (())restoreCompletedTransactions {
    // Убрали .unwrap()
    if !emulation_enabled() {
        let host_obj = env.objc.borrow::<SKPaymentQueueHostObject>(this);
        let observer = host_obj.observer;

        if observer != nil {
            // Вызываем метод делегата, сообщая, что "восстановление" успешно
            // завершено
            let _: () = msg![env; observer paymentQueueRestoreCompletedTransactionsFinished:this];
        }
        return;
    }
    let purchased = State::get(env).purchased.clone();
    let mut restored: Vec<id> = Vec::new();
    for identifier in &purchased {
        let payment = payment_for_identifier(env, identifier);
        let transaction_identifier = next_transaction_identifier(env, "restore");
        let transaction = make_transaction(
            env,
            SK_PAYMENT_TRANSACTION_STATE_RESTORED,
            payment,
            transaction_identifier,
            nil,
        );
        // The pending queue owns one reference; the pool guards delivery.
        retain(env, transaction);
        autorelease(env, transaction);
        env.objc
            .borrow_mut::<SKPaymentQueueHostObject>(this)
            .pending
            .push(transaction);
        restored.push(transaction);
    }
    log!(
        "SKPaymentQueue restoreCompletedTransactions: IAP emulation ON; restoring {} purchase(s).",
        restored.len()
    );
    deliver_transactions(env, this, &restored);
    let host_obj = env.objc.borrow::<SKPaymentQueueHostObject>(this);
    let observer = host_obj.observer;
    if observer != nil {
        let sel = env.objc.register_host_selector(
            "paymentQueueRestoreCompletedTransactionsFinished:".to_string(),
            &mut env.mem,
        );
        let responds: bool = msg![env; observer respondsToSelector:sel];
        if responds {
            let _: () =
                msg![env; observer paymentQueueRestoreCompletedTransactionsFinished:this];
        }
    }
}

- (())restoreCompletedTransactionsWithApplicationUsername:(id)_username {
    msg![env; this restoreCompletedTransactions]
}

- (())finishTransaction:(id)transaction { // SKPaymentTransaction*
    let was_pending = {
        let host = env.objc.borrow_mut::<SKPaymentQueueHostObject>(this);
        host.pending
            .iter()
            .position(|&t| t == transaction)
            .map(|pos| host.pending.remove(pos))
            .is_some()
    };
    if was_pending {
        release(env, transaction);
    }
    log_dbg!(
        "SKPaymentQueue finishTransaction: completed (was queued: {}).",
        was_pending
    );
}

// MARK: - Downloads (iOS 6+, always empty)

- (id)transactions {
    let pending = env
        .objc
        .borrow::<SKPaymentQueueHostObject>(this)
        .pending
        .clone();
    let array: id = msg_class![env; NSMutableArray array];
    for transaction in pending {
        () = msg![env; array addObject:transaction];
    }
    array
}

- (())startDownloads:(id)_downloads {
    log!("SKPaymentQueue startDownloads: stubbed");
}

- (())pauseDownloads:(id)_downloads {
    log!("SKPaymentQueue pauseDownloads: stubbed");
}

- (())resumeDownloads:(id)_downloads {
    log!("SKPaymentQueue resumeDownloads: stubbed");
}

- (())cancelDownloads:(id)_downloads {
    log!("SKPaymentQueue cancelDownloads: stubbed");
}

@end

// MARK: - SKPayment

#[derive(Default)]
struct SKPaymentHostObject {
    /// Retained identifier string, or nil.
    product_identifier: id,
}
impl HostObject for SKPaymentHostObject {}

@implementation SKPayment: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc
        .alloc_object(this, Box::<SKPaymentHostObject>::default(), &mut env.mem)
}

+ (id)paymentWithProductIdentifier:(id)identifier { // NSString*
    let payment: id = msg_class![env; SKPayment alloc];
    let payment: id = msg![env; payment initWithProductIdentifier:identifier];
    autorelease(env, payment)
}

+ (id)paymentWithProduct:(id)product { // SKProduct*
    if product == nil {
        return nil;
    }
    let identifier: id = msg![env; product productIdentifier];
    msg_class![env; SKPayment paymentWithProductIdentifier:identifier]
}

- (id)initWithProductIdentifier:(id)identifier {
    {
        let host = env.objc.borrow_mut::<SKPaymentHostObject>(this);
        host.product_identifier = identifier;
    }
    if identifier != nil {
        retain(env, identifier);
    }
    this
}

- (id)productIdentifier {
    let identifier = env
        .objc
        .borrow::<SKPaymentHostObject>(this)
        .product_identifier;
    if identifier == nil {
        ns_string::get_static_str(env, "")
    } else {
        retain(env, identifier);
        identifier
    }
}

- (NSInteger)quantity {
    1
}

- (())dealloc {
    let identifier = env
        .objc
        .borrow::<SKPaymentHostObject>(this)
        .product_identifier;
    release(env, identifier);
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

// MARK: - SKPaymentTransaction

@implementation SKPaymentTransaction: NSObject

- (NSInteger)transactionState {
    env.objc
        .borrow::<SKPaymentTransactionHostObject>(this)
        .state
}

- (id)transactionIdentifier {
    let identifier = env
        .objc
        .borrow::<SKPaymentTransactionHostObject>(this)
        .transaction_identifier;
    if identifier == nil {
        nil
    } else {
        retain(env, identifier);
        identifier
    }
}

- (id)payment {
    let payment = env
        .objc
        .borrow::<SKPaymentTransactionHostObject>(this)
        .payment;
    if payment == nil {
        nil
    } else {
        retain(env, payment);
        payment
    }
}

- (id)originalTransaction {
    let original = env
        .objc
        .borrow::<SKPaymentTransactionHostObject>(this)
        .original_transaction;
    if original == nil {
        nil
    } else {
        retain(env, original);
        original
    }
}

- (id)error {
    nil
}

- (())dealloc {
    let (transaction_identifier, payment, original_transaction) = {
        let host = env.objc.borrow::<SKPaymentTransactionHostObject>(this);
        (
            host.transaction_identifier,
            host.payment,
            host.original_transaction,
        )
    };
    release(env, transaction_identifier);
    release(env, payment);
    release(env, original_transaction);
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

};
