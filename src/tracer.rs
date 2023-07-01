use crate::callsite::GstCallsiteKind;
use glib::subclass::basic;
use gstreamer::{
    glib,
    prelude::PadExtManual,
    prelude::*,
    subclass::prelude::*,
    traits::{GstObjectExt, PadExt},
    Buffer, FlowError, FlowSuccess, Object, Pad, Tracer,
};
use std::cell::RefCell;
use tracing::{span::Attributes, Callsite, Dispatch, Id};

struct EnteredSpan {
    id: Id,
    dispatch: Dispatch,
}

pub struct TracingTracerPriv {
    span_stack: thread_local::ThreadLocal<RefCell<Vec<EnteredSpan>>>,
}

struct SpanBuilder<'a> {
    name: &'static str,
    pad: Option<&'a Pad>,
    event: Option<&'a gstreamer::Event>,
    query: Option<&'a gstreamer::QueryRef>,
}

impl<'a> SpanBuilder<'a> {
    fn new_pad(name: &'static str, pad: &'a Pad) -> Self {
        Self {
            name,
            pad: Some(pad),
            event: None,
            query: None,
        }
    }

    fn event(self, event: &'a gstreamer::Event) -> Self {
        Self {
            event: Some(event),
            ..self
        }
    }

    fn query(self, query: &'a gstreamer::QueryRef) -> Self {
        Self {
            query: Some(query),
            ..self
        }
    }

    fn build(self, tracer: &TracingTracerPriv) {
        let callsite = crate::callsite::DynamicCallsites::get().callsite_for(
            tracing::Level::ERROR,
            self.name,
            self.name,
            None,
            None,
            None,
            GstCallsiteKind::Span,
            self.pad.as_ref().map_or(
                unreachable!(),
                |_| {
                    // List of fields for pad traces
                    &[
                        "gstpad.flags",
                        "gstpad.name",
                        "gstpad.parent.name",
                        "gstpad.event.type",
                        "gstpad.query.type",
                    ]
                },
            ),
        );
        let interest = callsite.interest();
        if interest.is_never() {
            return;
        }
        let meta = callsite.metadata();
        let dispatch = tracing_core::dispatcher::get_default(move |dispatch| dispatch.clone());
        if !dispatch.enabled(meta) {
            return;
        }
        let pad = self.pad.unwrap();
        let gstpad_flags_value = Some(tracing_core::field::display(crate::PadFlags(
            pad.pad_flags().bits(),
        )));
        let name = pad.name();
        let name_value = Some(name.as_str());
        let gstpad_parent = pad.parent_element();
        let gstpad_parent_name_value = gstpad_parent.map(|p| p.name());
        let gstpad_parent_name_value = gstpad_parent_name_value.as_ref().map(|n| n.as_str());
        let fields = meta.fields();
        let mut fields_iter = fields.into_iter();
        let event_type = self.event.map(|e| e.type_().name());
        let event_type_name = event_type.as_ref().map(|n| n.as_str());
        let query_type_name = self.query.map(|q| crate::query_name(q));

        let values = field_values![fields_iter =>
            // /!\ /!\ /!\ Must be in the same order as the list of fields for pad above /!\ /!\ /!\
            "gstpad.flags" = gstpad_flags_value;
            "gstpad.name" = name_value;
            "gstpad.parent.name" = gstpad_parent_name_value;
            "gstpad.event.type" = event_type_name;
            "gstpad.query.type" = query_type_name;
        ];

        let valueset = fields.value_set(&values);
        let attrs = tracing::span::Attributes::new_root(meta, &valueset);
        tracer.push_span(dispatch, attrs);
    }
}

impl TracingTracerPriv {
    fn push_span(&self, dispatch: Dispatch, attributes: Attributes) {
        let span_id = dispatch.new_span(&attributes);
        dispatch.enter(&span_id);
        self.span_stack
            .get_or(|| RefCell::new(Vec::new()))
            .borrow_mut()
            .push(EnteredSpan {
                id: span_id,
                dispatch,
            })
    }
    fn pop_span(&self) -> Option<()> {
        self.span_stack
            .get_or(|| RefCell::new(Vec::new()))
            .borrow_mut()
            .pop()
            .map(|span| {
                span.dispatch.exit(&span.id);
                span.dispatch.try_close(span.id);
            })
    }

    fn pad_pre(&self, name: &'static str, pad: &Pad) {
        let builder = SpanBuilder::new_pad(name, pad);

        builder.build(self);
    }
}

glib::wrapper! {
    pub struct TracingTracer(ObjectSubclass<TracingTracerPriv>)
       @extends Tracer, Object;
}

#[glib::object_subclass]
impl ObjectSubclass for TracingTracerPriv {
    const NAME: &'static str = "TracingTracer";
    type Type = TracingTracer;
    type Class = basic::ClassStruct<Self>;
    type Instance = basic::InstanceStruct<Self>;
    type ParentType = Tracer;
    type Interfaces = ();

    fn new() -> Self {
        Self {
            span_stack: thread_local::ThreadLocal::new(),
        }
    }
}

impl ObjectImpl for TracingTracerPriv {
    fn constructed(&self) {
        self.parent_constructed();
        self.register_hook(TracerHook::PadPushPost);
        self.register_hook(TracerHook::PadPushPre);
        self.register_hook(TracerHook::PadPushListPost);
        self.register_hook(TracerHook::PadPushListPre);
        self.register_hook(TracerHook::PadQueryPost);
        self.register_hook(TracerHook::PadQueryPre);
        self.register_hook(TracerHook::PadPushEventPost);
        self.register_hook(TracerHook::PadPushEventPre);
        self.register_hook(TracerHook::PadPullRangePost);
        self.register_hook(TracerHook::PadPullRangePre);

        #[cfg(feature = "v1_22")]
        {
            self.register_hook(TracerHook::PadChainPre);
            self.register_hook(TracerHook::PadChainPost);
        }
    }
}

impl GstObjectImpl for TracingTracerPriv {}

impl TracerImpl for TracingTracerPriv {
    #[cfg(feature = "v1_22")]
    fn pad_chain_pre(&self, _: u64, pad: &Pad, _: &Buffer) {
        self.pad_pre("pad_chain", pad);
    }

    fn pad_push_pre(&self, _: u64, pad: &Pad, _: &Buffer) {
        self.pad_pre("pad_push", pad);
    }

    fn pad_push_list_pre(&self, _: u64, pad: &Pad, _: &gstreamer::BufferList) {
        self.pad_pre("pad_push_list", pad);
    }

    fn pad_query_pre(&self, _: u64, pad: &Pad, query: &gstreamer::QueryRef) {
        SpanBuilder::new_pad("pad_query", pad)
            .query(query)
            .build(self);
    }

    fn pad_push_event_pre(&self, _: u64, pad: &Pad, event: &gstreamer::Event) {
        SpanBuilder::new_pad("pad_event", pad)
            .event(event)
            .build(self);
    }

    fn pad_pull_range_pre(&self, _: u64, pad: &Pad, _: u64, _: u32) {
        self.pad_pre("pad_pull_range", pad);
    }

    fn pad_pull_range_post(&self, _: u64, _: &Pad, _: Result<&Buffer, FlowError>) {
        self.pop_span();
    }

    fn pad_push_event_post(&self, _: u64, _: &Pad, _: bool) {
        self.pop_span();
    }

    fn pad_push_list_post(&self, _: u64, _: &Pad, _: Result<FlowSuccess, FlowError>) {
        self.pop_span();
    }

    fn pad_push_post(&self, _: u64, _: &Pad, _: Result<FlowSuccess, FlowError>) {
        self.pop_span();
    }

    #[cfg(feature = "v1_22")]
    fn pad_chain_post(&self, _: u64, _: &Pad, _: Result<FlowSuccess, FlowError>) {
        self.pop_span();
    }

    fn pad_query_post(&self, _: u64, _: &Pad, _: &gstreamer::QueryRef, _: bool) {
        self.pop_span();
    }
}
