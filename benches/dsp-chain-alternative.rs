#![feature(test)]
extern crate test;

#[macro_use]
extern crate lazy_static;

use dsp::Node;
use sample::*;
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use test::Bencher;

use audiograph;
use dsp;

lazy_static! {
    static ref TEST_DATA: Vec<f32> = {
        let mut test: Vec<f32> = vec![0.; std::u16::MAX as usize];
        for (index, value) in test.iter_mut().enumerate() {
            *value = index as f32;
        }
        test
    };
}

#[bench]
fn bench_dsp_chain_count_to_max(b: &mut Bencher) {
    struct CountingNode {
        current: f32,
    }

    impl dsp::Node<[f32; 1]> for CountingNode {
        fn audio_requested(&mut self, buffer: &mut [[f32; 1]], _sample_hz: f64) {
            for sample in buffer.iter_mut() {
                println!("counting");

                sample[0] = self.current;
                self.current += 1.;
            }
        }
    }

    let test: Vec<[f32; 1]> = TEST_DATA.iter().cloned().map(|x| [x; 1]).collect();

    b.iter(|| {
        let mut buffer: Vec<[f32; 1]> = vec![[0.; 1]; std::u16::MAX as usize];
        let mut graph = dsp::Graph::new();
        let counter = graph.add_node(CountingNode { current: 0. });
        graph.set_master(Some(counter));
        graph.audio_requested(&mut buffer, 44100.0);
        assert_eq!(buffer, test);
    });
}

#[bench]
fn bench_audiograph_count_to_max(b: &mut Bencher) {
    #[derive(Debug)]
    struct CountingNode {
        current: f32,
    }

    impl audiograph::Route<f32> for CountingNode {
        fn process(
            &mut self,
            _input: &[audiograph::BufferPoolReference<f32>],
            output: &mut [audiograph::BufferPoolReference<f32>],
            _frames: usize,
        ) {
            for sample in output[0].as_mut().iter_mut() {
                *sample = self.current;
                self.current += 1.;
            }
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug)]
    struct OutputNode {
        buffer: Rc<RefCell<Vec<f32>>>,
    }

    impl audiograph::Route<f32> for OutputNode {
        fn process(
            &mut self,
            input: &[audiograph::BufferPoolReference<f32>],
            _output: &mut [audiograph::BufferPoolReference<f32>],
            _frames: usize,
        ) {
            let mut buffer = self.buffer.borrow_mut();
            for (input, output) in input[0].as_ref().iter().zip(buffer.iter_mut()) {
                *output = *input;
            }
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
    struct Id(u32);

    impl audiograph::NodeId for Id {
        fn generate_node_id() -> Self {
            Id(0)
        }
    }

    let test: Vec<f32> = TEST_DATA.iter().cloned().collect();

    b.iter(|| {
        let buffer: Vec<f32> = vec![0.; std::u16::MAX as usize];
        let buffer = Rc::new(RefCell::new(buffer));

        let output_id = Id(1);

        let buffer_size = std::u16::MAX;

        let mut graph = audiograph::RouteGraphBuilder::new()
            .with_buffer_size(buffer_size as usize)
            .build();

        graph.add_node(audiograph::Node::new(
            1,
            Box::new(CountingNode { current: 0. }),
            vec![audiograph::Connection::new(output_id, 1.)],
        ));

        graph.add_node(audiograph::Node::with_id(
            output_id,
            1,
            Box::new(OutputNode {
                buffer: Rc::clone(&buffer),
            }),
            vec![],
        ));

        graph.process(buffer_size as usize);

        assert_eq!(*buffer.borrow(), test);
    });
}
