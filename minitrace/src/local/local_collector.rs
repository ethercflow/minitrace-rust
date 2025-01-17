// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use minstant::Instant;

use crate::local::local_span_stack::LocalSpanStack;
use crate::local::local_span_stack::SpanLineHandle;
use crate::local::local_span_stack::LOCAL_SPAN_STACK;
use crate::util::CollectToken;
use crate::util::RawSpans;

/// Collector to collect [`LocalSpan`].
///
/// `LocalCollector` allows to collect `LocalSpan` manually without a local parent. The collected `LocalSpan` can later be
/// attached to a parent.
///
/// Generally, [`Span`] and `LocalSpan` are sufficient. However, use `LocalCollector` when the span might initiate before its
/// parent span. This is particularly useful for tracing prior tasks that may be obstructing the current request.
///
/// # Examples
///
/// ```
/// use minitrace::local::LocalCollector;
/// use minitrace::prelude::*;
///
/// // Collect local spans manually without a parent
/// let collector = LocalCollector::start();
/// let span = LocalSpan::enter_with_local_parent("a child span");
/// drop(span);
/// let local_spans = collector.collect();
///
/// // Attach the local spans to a parent
/// let root = Span::root("root", SpanContext::new(TraceId(12), SpanId::default()));
/// root.push_child_spans(local_spans);
/// ```
///
/// [`Span`]: crate::Span
/// [`LocalSpan`]: crate::local::LocalSpan
#[must_use]
pub struct LocalCollector {
    #[cfg(feature = "enable")]
    inner: Option<LocalCollectorInner>,
}

struct LocalCollectorInner {
    stack: Rc<RefCell<LocalSpanStack>>,
    span_line_handle: SpanLineHandle,
}

/// A collection of [`LocalSpan`] instances.
///
/// This struct is typically used to group together multiple `LocalSpan` instances that have been
/// collected from a [`LocalCollector`]. These spans can then be associated with a parent span using
/// the [`Span::push_child_spans()`] method on the parent span.
///
/// Internally, it is implemented as an `Arc<[LocalSpan]>`, which allows it to be cloned and shared
/// across threads at a low cost.
///
/// # Examples
///
/// ```
/// use minitrace::local::LocalCollector;
/// use minitrace::local::LocalSpans;
/// use minitrace::prelude::*;
///
/// // Collect local spans manually without a parent
/// let collector = LocalCollector::start();
/// let span = LocalSpan::enter_with_local_parent("a child span");
/// drop(span);
///
/// // Collect local spans into a LocalSpans instance
/// let local_spans: LocalSpans = collector.collect();
///
/// // Attach the local spans to a parent
/// let root = Span::root("root", SpanContext::new(TraceId(12), SpanId::default()));
/// root.push_child_spans(local_spans);
/// ```
///
/// [`Span::push_child_spans()`]: crate::Span::push_child_spans()
/// [`LocalSpan`]: crate::local::LocalSpan
/// [`LocalCollector`]: crate::local::LocalCollector
#[derive(Debug, Clone)]
pub struct LocalSpans {
    #[cfg(feature = "enable")]
    pub(crate) inner: Arc<LocalSpansInner>,
}

#[derive(Debug)]
pub struct LocalSpansInner {
    pub spans: RawSpans,
    pub end_time: Instant,
}

impl LocalCollector {
    pub fn start() -> Self {
        #[cfg(not(feature = "enable"))]
        {
            LocalCollector {}
        }

        #[cfg(feature = "enable")]
        {
            let stack = LOCAL_SPAN_STACK.with(Rc::clone);
            Self::new(None, stack)
        }
    }

    pub fn collect(self) -> LocalSpans {
        #[cfg(not(feature = "enable"))]
        {
            LocalSpans {}
        }

        #[cfg(feature = "enable")]
        {
            LocalSpans {
                inner: Arc::new(self.collect_spans_and_token().0),
            }
        }
    }
}

#[cfg(feature = "enable")]
impl LocalCollector {
    pub(crate) fn new(
        collect_token: Option<CollectToken>,
        stack: Rc<RefCell<LocalSpanStack>>,
    ) -> Self {
        let span_line_epoch = {
            let stack = &mut (*stack).borrow_mut();
            stack.register_span_line(collect_token)
        };

        Self {
            inner: span_line_epoch.map(move |span_line_handle| LocalCollectorInner {
                stack,
                span_line_handle,
            }),
        }
    }

    pub(crate) fn collect_spans_and_token(mut self) -> (LocalSpansInner, Option<CollectToken>) {
        let (spans, collect_token) = self
            .inner
            .take()
            .and_then(
                |LocalCollectorInner {
                     stack,
                     span_line_handle,
                 }| {
                    let s = &mut (*stack).borrow_mut();
                    s.unregister_and_collect(span_line_handle)
                },
            )
            .unwrap_or_default();

        (
            LocalSpansInner {
                spans,
                end_time: Instant::now(),
            },
            collect_token,
        )
    }
}

impl Drop for LocalCollector {
    fn drop(&mut self) {
        #[cfg(feature = "enable")]
        if let Some(LocalCollectorInner {
            stack,
            span_line_handle,
        }) = self.inner.take()
        {
            let s = &mut (*stack).borrow_mut();
            let _ = s.unregister_and_collect(span_line_handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::CollectTokenItem;
    use crate::collector::SpanId;
    use crate::prelude::TraceId;
    use crate::util::tree::tree_str_from_raw_spans;

    #[test]
    fn local_collector_basic() {
        let stack = Rc::new(RefCell::new(LocalSpanStack::with_capacity(16)));
        let collector1 = LocalCollector::new(None, stack.clone());

        let span1 = stack.borrow_mut().enter_span("span1").unwrap();
        {
            let token2 = CollectTokenItem {
                trace_id: TraceId(1234),
                parent_id: SpanId::default(),
                collect_id: 42,
                is_root: false,
            };
            let collector2 = LocalCollector::new(Some(token2.into()), stack.clone());
            let span2 = stack.borrow_mut().enter_span("span2").unwrap();
            let span3 = stack.borrow_mut().enter_span("span3").unwrap();
            stack.borrow_mut().exit_span(span3);
            stack.borrow_mut().exit_span(span2);

            let (spans, token) = collector2.collect_spans_and_token();
            assert_eq!(token.unwrap().as_slice(), &[token2]);
            assert_eq!(
                tree_str_from_raw_spans(spans.spans),
                r"
span2 []
    span3 []
"
            );
        }
        stack.borrow_mut().exit_span(span1);
        let spans = collector1.collect();
        assert_eq!(
            tree_str_from_raw_spans(spans.inner.spans.iter().cloned().collect()),
            r"
span1 []
"
        );
    }

    #[test]
    fn drop_without_collect() {
        let stack = Rc::new(RefCell::new(LocalSpanStack::with_capacity(16)));
        let collector1 = LocalCollector::new(None, stack.clone());

        let span1 = stack.borrow_mut().enter_span("span1").unwrap();
        {
            let token2 = CollectTokenItem {
                trace_id: TraceId(1234),
                parent_id: SpanId::default(),
                collect_id: 42,
                is_root: false,
            };
            let collector2 = LocalCollector::new(Some(token2.into()), stack.clone());
            let span2 = stack.borrow_mut().enter_span("span2").unwrap();
            let span3 = stack.borrow_mut().enter_span("span3").unwrap();
            stack.borrow_mut().exit_span(span3);
            stack.borrow_mut().exit_span(span2);
            drop(collector2);
        }
        stack.borrow_mut().exit_span(span1);
        let spans = collector1.collect();
        assert_eq!(
            tree_str_from_raw_spans(spans.inner.spans.iter().cloned().collect()),
            r"
span1 []
"
        );
    }
}
