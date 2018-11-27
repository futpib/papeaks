
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate arrayref;
extern crate clap;
extern crate libpulse_binding as pulse;

use std::sync::Mutex;
use std::rc::Rc;
use std::cell::RefCell;
use std::ops::Deref;
use std::collections::HashSet;
use std::collections::HashMap;
use std::io::Write;

use clap::{Arg, App};

use pulse::mainloop::standard::Mainloop;
use pulse::context::Context;
use pulse::context::subscribe::subscription_masks;
use pulse::context::subscribe::Facility;
use pulse::context::subscribe::Operation;
use pulse::stream::Stream;
use pulse::stream::PeekResult;
use pulse::proplist::Proplist;
use pulse::mainloop::standard::IterateResult;
use pulse::def::Retval;
use pulse::callbacks::ListResult;
use pulse::context::introspect::SinkInfo;
use pulse::context::introspect::SinkInputInfo;

lazy_static! {
    static ref sources: Mutex<HashSet<u32>> = Mutex::new(HashSet::new());
    static ref sink_inputs: Mutex<HashSet<u32>> = Mutex::new(HashSet::new());

    static ref sink_input_info_queue: Mutex<HashSet<u32>> = Mutex::new(HashSet::new());
    static ref sink_info_queue: Mutex<HashSet<u32>> = Mutex::new(HashSet::new());

    static ref sink_input_to_sink: Mutex<HashMap<u32, u32>> = Mutex::new(HashMap::new());
    static ref sink_to_monitor: Mutex<HashMap<u32, u32>> = Mutex::new(HashMap::new());
}

fn create_monitor_stream_for_source(context: &Rc<RefCell<Context>>, source_index: Option<u32>, stream_index: Option<u32>) -> Rc<RefCell<Stream>> {
    let spec = pulse::sample::Spec {
        format: pulse::sample::SAMPLE_FLOAT32,
        channels: 1,
        rate: 25,
    };
    assert!(spec.is_valid());

    let stream = Rc::new(RefCell::new(Stream::new(
        &mut context.borrow_mut(),
        "papeaks",
        &spec,
        None
    ).expect("Failed to create new stream")));

    if let Some(index) = stream_index {
        stream.borrow_mut().set_monitor_stream(index).unwrap();
    }

    let s = match source_index {
        Some(i) => i.to_string(),
        None => String::new(),
    };

    stream.borrow_mut().connect_record(
        if source_index.is_some() { Some(&s.as_str()) } else { None },
        None,
        pulse::stream::flags::PEAK_DETECT | pulse::stream::flags::ADJUST_LATENCY,
    ).expect("Failed to connect recording");

    return stream;
}

fn on_sink_input_info (res: ListResult<&SinkInputInfo>) {
    match res {
        ListResult::Item(sink_input) => {
            sink_inputs.lock().unwrap().insert(sink_input.index);
            sink_input_to_sink.lock().unwrap().insert(sink_input.index, sink_input.sink);
            sink_info_queue.lock().unwrap().insert(sink_input.sink);
            eprintln!("SinkInputInfo {} {}", sink_input.index, sink_input.sink);
        },
        _ => {},
    }
}

fn on_sink_info (res: ListResult<&SinkInfo>) {
    match res {
        ListResult::Item(sink) => {
            sink_to_monitor.lock().unwrap().insert(sink.index, sink.monitor_source);
            eprintln!("SinkInfo {} {}", sink.index, sink.monitor_source);
        },
        _ => {},
    }
}

trait Outputter {
    fn start(&mut self);
    fn peak(&mut self, &Facility, &u32, &[u8; 4]);
    fn end(&mut self);
}

struct Binary;
struct Unix;

impl Outputter for Binary {
    fn start(&mut self) {}
    fn peak(&mut self, object_type: &Facility, object_index: &u32, peak: &[u8; 4]) {
        let mut out = std::io::stdout();
        unsafe {
            out.write(std::mem::transmute::<&u32, &[u8; 4]>(object_index)).unwrap();
            out.write(&std::mem::transmute::<u32, [u8; 4]>(object_type.to_interest_mask())).unwrap();
        }
        out.write(peak).unwrap();
        out.flush().unwrap();
    }
    fn end(&mut self) {}
}

impl Outputter for Unix {
    fn start(&mut self) {}
    fn peak(&mut self, object_type_: &Facility, object_index: &u32, peak_: &[u8; 4]) {
        let peak = unsafe { std::mem::transmute::<&[u8; 4], &f32>(peak_) };
        let object_type = match object_type_ {
            Facility::Source => "source",
            Facility::SinkInput => "sink_input",
            _ => panic!("unexpected facility {:?}", object_type_),
        };
        println!("{} {} {}", object_type, object_index, peak);
    }
    fn end(&mut self) {}
}

fn output(data: &[u8], facility: &Facility, object_index: &u32, outputter: &mut Box<dyn Outputter>) {
    for chunk in data.chunks(4) {
        if chunk.len() == 4 {
            unsafe {
                outputter.peak(facility, object_index, array_ref![chunk, 0, 4]);
            }
        }
    }
}

fn main() {
    let matches = App::new("papeaks")
        .arg(Arg::with_name("output")
                               .long("output")
                               .help("Output format. One of `binary` or `unix` (default).")
                               .takes_value(true))
        .get_matches();

    let mut outputter: Box<dyn Outputter> = match matches.value_of("output").unwrap_or("unix") {
        "binary" => Box::new(Binary),
        _ => Box::new(Unix),
    };

    let mut proplist = Proplist::new().unwrap();
    proplist.sets(pulse::proplist::properties::APPLICATION_NAME, "papeaks")
        .unwrap();

    let mainloop = Rc::new(RefCell::new(Mainloop::new()
        .expect("Failed to create mainloop")));

    let context: Rc<RefCell<Context>> = Rc::new(RefCell::new(Context::new_with_proplist(
        mainloop.borrow().deref(),
        "papeaks",
        &proplist
        ).expect("Failed to create new context")));

    context.borrow_mut().connect(None, pulse::context::flags::NOFLAGS, None)
        .expect("Failed to connect context");

    // Wait for context to be ready
    loop {
        match mainloop.borrow_mut().iterate(true) {
            IterateResult::Quit(_) |
            IterateResult::Err(_) => {
                eprintln!("iterate state was not success, quitting...");
                return;
            },
            IterateResult::Success(_) => {},
        }
        match context.borrow().get_state() {
            pulse::context::State::Ready => { break; },
            pulse::context::State::Failed |
            pulse::context::State::Terminated => {
                eprintln!("context state failed/terminated, quitting...");
                return;
            },
            _ => {},
        }
    }

    context.borrow_mut().subscribe(
        subscription_masks::SOURCE | subscription_masks::SINK_INPUT,
        |success: bool| {
            assert!(success, "subscription failed");
        },
    );

    context.borrow().introspect().get_source_info_list(|res| {
        match res {
            ListResult::Item(source) => {
                sources.lock().unwrap().insert(source.index);
            },
            _ => {},
        }
    });

    context.borrow().introspect().get_sink_input_info_list(on_sink_input_info);

    context.borrow_mut().set_subscribe_callback(Some(Box::new(|facility, operation, index| {
        eprintln!("{:?} {:?} {:?}", facility, operation, index);
        match facility {
            Some(Facility::Source) => match operation {
                Some(Operation::New) => { sources.lock().unwrap().insert(index); },
                Some(Operation::Removed) => { sources.lock().unwrap().remove(&index); },
                _ => {},
            },
            Some(Facility::SinkInput) => match operation {
                Some(Operation::New) => {
                    sink_inputs.lock().unwrap().insert(index);
                    sink_input_info_queue.lock().unwrap().insert(index);
                },
                Some(Operation::Changed) => {
                    sink_inputs.lock().unwrap().insert(index);
                    sink_input_info_queue.lock().unwrap().insert(index);
                },
                Some(Operation::Removed) => {
                    sink_inputs.lock().unwrap().remove(&index);
                    sink_input_info_queue.lock().unwrap().remove(&index);
                },
                _ => {},
            },
            _ => {},
        };
    })));

    let mut source_recorders: HashMap<u32, Rc<RefCell<Stream>>> = HashMap::new();
    let mut sink_input_recorders: HashMap<u32, Rc<RefCell<Stream>>> = HashMap::new();

    outputter.start();

    loop {
        match mainloop.borrow_mut().iterate(true) {
            IterateResult::Quit(_) |
            IterateResult::Err(_) => {
                eprintln!("iterate state was not success, quitting...");
                break;
            },
            IterateResult::Success(_) => {},
        }

        sink_input_info_queue.lock().unwrap().retain(|sink_input_index| {
            context.borrow().introspect().get_sink_input_info(*sink_input_index, on_sink_input_info);
            false
        });

        sink_info_queue.lock().unwrap().retain(|sink_index| {
            context.borrow().introspect().get_sink_info_by_index(*sink_index, on_sink_info);
            false
        });

        source_recorders.retain(|source_index, recorder| {
            if !sources.lock().unwrap().contains(source_index) {
                return false;
            }
            match recorder.borrow().get_state() {
                pulse::stream::State::Failed => false,
                _ => true
            }
        });

        for source_index in sources.lock().unwrap().iter() {
            if source_recorders.contains_key(source_index) {
                continue;
            }
            source_recorders.insert(*source_index, create_monitor_stream_for_source(&context, Some(*source_index), None));
        }

        for sink_input_index in sink_inputs.lock().unwrap().iter() {
            if sink_input_recorders.contains_key(sink_input_index) {
                continue;
            }
            if let Some(sink_index) = sink_input_to_sink.lock().unwrap().get(sink_input_index) {
                if let Some(monitor_index) = sink_to_monitor.lock().unwrap().get(sink_index) {
                    sink_input_recorders.insert(*sink_input_index, create_monitor_stream_for_source(&context, Some(*monitor_index), Some(*sink_input_index)));
                }
            }
        }

        sink_input_recorders.retain(|sink_input_index, recorder| {
            if !sink_inputs.lock().unwrap().contains(sink_input_index) {
                return false;
            }
            match recorder.borrow().get_state() {
                pulse::stream::State::Failed => false,
                _ => true
            }
        });

        for (source_index, recorder) in source_recorders.iter() {
            let stream_state = recorder.borrow().get_state();
            match stream_state {
                pulse::stream::State::Failed => {
                    eprintln!("peaks Stream for Source {} failed", source_index);
                },
                pulse::stream::State::Terminated => {
                    eprintln!("peaks Stream for Source {} terminated", source_index);
                    sources.lock().unwrap().remove(source_index);
                },
                pulse::stream::State::Ready => {
                    let mut stream = recorder.borrow_mut();
                    match stream.peek() {
                        Ok(res) => match res {
                            PeekResult::Data(data) => {
                                output(data, &Facility::Source, source_index, &mut outputter);
                                stream.discard().unwrap();
                            },
                            PeekResult::Hole(_) => {
                                stream.discard().unwrap();
                            },
                            _ => {},
                        },
                        Err(err) => {
                            eprintln!("err: {:?}", err);
                            sources.lock().unwrap().remove(source_index);
                        },
                    }
                },
                _ => {},
            }
        }

        for (sink_input_index, recorder) in sink_input_recorders.iter() {
            let stream_state = recorder.borrow().get_state();
            match stream_state {
                pulse::stream::State::Failed => {
                    eprintln!("peaks Stream for SinkInput {} failed", sink_input_index);
                },
                pulse::stream::State::Terminated => {
                    eprintln!("peaks Stream for SinkInput {} terminated", sink_input_index);
                    sink_inputs.lock().unwrap().remove(sink_input_index);
                },
                pulse::stream::State::Ready => {
                    let mut stream = recorder.borrow_mut();
                    match stream.peek() {
                        Ok(res) => match res {
                            PeekResult::Data(data) => {
                                output(data, &Facility::SinkInput, sink_input_index, &mut outputter);
                                stream.discard().unwrap();
                            },
                            PeekResult::Hole(_) => {
                                stream.discard().unwrap();
                            },
                            _ => {},
                        },
                        Err(err) => {
                            eprintln!("err: {:?}", err);
                            sink_inputs.lock().unwrap().remove(sink_input_index);
                        },
                    }
                },
                _ => {},
            }
        }
    }

    outputter.end();

    mainloop.borrow_mut().quit(Retval(0));
}

