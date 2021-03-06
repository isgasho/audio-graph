extern crate bufferpool;

mod arena;
pub mod builder;
pub mod node;

pub use builder::*;
pub use node::*;

use crate::route::Route;
use generational_arena::{Arena, Index};
use sample::Sample;
use std::collections::HashSet;

use arena::{insert_with, split_at};

use bufferpool::{BufferPool, BufferPoolBuilder, BufferPoolReference};

pub struct RouteGraph<S: Sample + Default, R> {
    ordering: Vec<Index>,
    visited: HashSet<Index>,
    temp: Vec<BufferPoolReference<S>>,
    arena: Arena<Node<S, R>>,
    max_channels: usize,
    pool: BufferPool<S>,
    sorted: bool,
}

// Implement Send and Sync if all the routes are Send.
// The problem is buffer pool - which has a bunch of mutable
// references and such. But RouteGraph should be fine to send
// between threads so long as it's routes are safe to send
// between threads.
unsafe impl<S, R, C> Send for RouteGraph<S, R>
where
    S: Sample + Default,
    R: Route<S, Context = C> + Send,
{
}

unsafe impl<S, R, C> Sync for RouteGraph<S, R>
where
    S: Sample + Default,
    R: Route<S, Context = C> + Send,
{
}

impl<S, R, C> Default for RouteGraph<S, R>
where
    S: Sample + Default,
    R: Route<S, Context = C>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S, R, C> From<Arena<Node<S, R>>> for RouteGraph<S, R>
where
    S: Sample + Default,
    R: Route<S, Context = C>,
{
    fn from(arena: Arena<Node<S, R>>) -> Self {
        Self::build(arena, 1024)
    }
}

impl<S, R, C> RouteGraph<S, R>
where
    S: Sample + Default,
    R: Route<S, Context = C>,
{
    pub(crate) fn build(arena: Arena<Node<S, R>>, buffer_size: usize) -> Self {
        let ordering: Vec<Index> = Vec::with_capacity(arena.len());

        let capacity = arena.len();
        let max_channels = arena.iter().fold(0, |a, (_, b)| a.max(b.channels));

        let mut graph = Self {
            ordering,
            arena,
            visited: HashSet::with_capacity(capacity),
            temp: Vec::with_capacity(max_channels),
            max_channels,
            pool: BufferPoolBuilder::new()
                .with_capacity(0)
                .with_buffer_size(0)
                .build(),
            sorted: false,
        };

        graph.topographic_sort();

        let buffer_count = graph.count_required_temp_buffers();

        graph.pool = BufferPoolBuilder::new()
            .with_capacity(buffer_count + max_channels)
            .with_buffer_size(buffer_size)
            .build();

        graph
    }

    fn process_parts<I: Iterator<Item = usize>>(&mut self, ranges: I, context: &mut C) {
        let temp = &mut self.temp;
        let arena = &mut self.arena;

        let pool = &mut self.pool;

        let ordering = &self.ordering;

        for frames in ranges {
            for id in ordering {
                if let Some((current, mut rest)) = split_at(arena, *id) {
                    let buffers = &current.buffers;
                    let node_route = &mut current.route;
                    let connections = &current.connections;

                    node_route.process(buffers, temp, frames, context);

                    for send in connections {
                        if let Some(out_route) = rest.get_mut(send.id) {
                            if out_route.buffers.len() < out_route.channels {
                                for _ in 0..(out_route.channels - out_route.buffers.len()) {
                                    out_route.buffers.push(pool.get_cleared_space().unwrap());
                                }
                            }

                            for (output_vector, input_vector) in
                                out_route.buffers.iter_mut().zip(temp.iter())
                            {
                                for (output, input) in output_vector
                                    .as_mut()
                                    .iter_mut()
                                    .zip(input_vector.as_ref().iter())
                                {
                                    *output = output.add_amp(
                                        input
                                            .mul_amp(send.amount.to_float_sample())
                                            .to_signed_sample(),
                                    );
                                }
                            }
                        }
                    }

                    current.buffers.drain(..).for_each(drop);
                }
            }
        }
    }

    pub fn process(&mut self, frames: usize, context: &mut C) {
        let buffer_size = self.buffer_size();

        {
            let temp = &mut self.temp;
            let pool = &mut self.pool;

            for _ in 0..self.max_channels {
                temp.push(pool.get_space().unwrap());
            }
        }

        if buffer_size >= frames {
            let range = (0..1).map(|_| frames);
            self.process_parts(range, context)
        } else {
            let range = (0..=((frames + buffer_size - 1) / buffer_size))
                .map(|i| (frames - ((i.max(1) - 1) * buffer_size)).min(buffer_size));
            self.process_parts(range, context)
        }

        self.temp.drain(..).for_each(drop);
    }

    /// Change the graph buffer size
    ///
    /// # Panics
    /// If any of the internal buffers have been borrowed
    pub fn set_buffer_size(&mut self, buffer: usize) {
        self.pool.change_buffer_size(buffer);
    }

    pub fn buffer_size(&self) -> usize {
        self.pool.get_buffer_size()
    }

    pub fn is_sorted(&self) -> bool {
        self.sorted
    }

    // TODO: Add better new Method
    pub fn new() -> Self {
        RouteGraph {
            ordering: vec![],
            visited: HashSet::new(),
            temp: vec![],
            arena: Arena::new(),
            pool: BufferPool::default(),

            max_channels: 0,
            sorted: true,
        }
    }

    fn count_buffers_for_node(&self, node: &Node<S, R>) -> usize {
        let connections = &node.connections;

        let mut count = node.channels;

        for send in connections {
            if let Some(out_route) = self.arena.get(send.id) {
                count += out_route.channels;
            }
        }

        count
    }

    fn count_required_temp_buffers(&self) -> usize {
        let mut count: usize = 0;
        let mut max: usize = 0;

        for (_, node) in self
            .ordering
            .iter()
            .filter_map(|id| self.arena.get(*id).map(|node| (*id, node)))
        {
            count += self.count_buffers_for_node(node);
            max = max.max(count);
            count -= node.channels.min(count);
        }

        max
    }

    fn topographic_sort_inner(
        visited: &mut HashSet<Index>,
        output: &mut Vec<Index>,
        arena: &Arena<Node<S, R>>,
        input: &Node<S, R>,
    ) {
        visited.insert(input.id());

        for Connection { id, .. } in input.connections.iter() {
            if !visited.contains(id) {
                if let Some(node) = arena.get(*id) {
                    Self::topographic_sort_inner(visited, output, arena, node);
                }
            }
        }

        output.push(input.id());
    }

    pub fn topographic_sort(&mut self) {
        // Set all visited elements to false
        let visited = &mut (self.visited);
        visited.clear();

        let ordering = &mut (self.ordering);
        ordering.truncate(0);

        for (_, node) in self.arena.iter() {
            Self::topographic_sort_inner(visited, ordering, &self.arena, node);
        }

        ordering.reverse();
        assert_eq!(ordering.len(), self.arena.len());

        self.sorted = true;
    }

    pub fn silence_all_buffers(&mut self) {
        self.pool.clear();
    }

    pub fn len(&self) -> usize {
        self.arena.len()
    }

    // Set the volume / amount of a particular route
    pub fn set_route_amount(&mut self, source: Index, target: Index, amount: S) {
        self.with_node_connections(source, |connections| {
            if let Some(position) = connections.iter().position(|c| &c.id == &target) {
                if amount == S::equilibrium() {
                    connections.swap_remove(position);
                } else {
                    connections.get_mut(position).unwrap().amount = amount;
                }
            } else {
                if amount != S::equilibrium() {
                    connections.push(Connection::new(target, amount))
                }
            }
        });
    }

    pub fn with_node_mut<T, F: FnOnce(&mut Node<S, R>) -> T>(
        &mut self,
        id: Index,
        func: F,
    ) -> Option<T> {
        self.arena.get_mut(id).map(func)
    }

    pub fn with_node<T, F: FnOnce(&Node<S, R>) -> T>(&self, id: Index, func: F) -> Option<T> {
        self.arena.get(id).map(func)
    }

    pub fn with_node_connections<T, F: FnOnce(&mut Vec<Connection<S>>) -> T>(
        &mut self,
        id: Index,
        func: F,
    ) -> Option<T> {
        self.with_node_mut(id, |node| func(&mut node.connections))
    }

    pub fn remove_node(&mut self, id: Index) -> Option<Node<S, R>> {
        let node = self.arena.remove(id);

        for (_, node) in self.arena.iter_mut() {
            node.connections.retain(|connection| &connection.id != &id);
        }

        self.sorted = false;

        node
    }

    pub fn add_node_with_idx<F: Send + FnMut(Index) -> Node<S, R>>(
        &mut self,
        mut func: F,
    ) -> Index {
        let id = insert_with(&mut self.arena, |id| func(id));

        self.pool.reserve(1);
        self.visited.reserve(1);

        let (buffers, max_channels) = self
            .with_node(id, |node| {
                (self.count_buffers_for_node(node), node.channels)
            })
            .unwrap();

        self.max_channels = self.max_channels.max(max_channels);

        let temp_capacity = self.temp.capacity();

        self.temp
            .reserve(temp_capacity.max(self.max_channels) - temp_capacity);

        let pool_capacity = self.pool.capacity();

        self.pool
            .reserve((buffers + self.max_channels).max(pool_capacity) - pool_capacity);

        self.sorted = false;

        self.ordering.push(id);

        id
    }

    pub fn has_cycles(&mut self) -> bool {
        let ordering = &self.ordering;
        let arena = &self.arena;
        let visited = &mut (self.visited);
        visited.clear();

        for (id, route) in ordering
            .iter()
            .filter_map(|id| arena.get(*id).map(|node| (*id, node)))
        {
            visited.insert(id);

            for out in &route.connections {
                if visited.contains(&out.id) {
                    self.sorted = false;
                    return true;
                }
            }
        }

        self.sorted = true;

        false
    }
}

#[cfg(test)]
mod tests {
    use alloc_counter::{deny_alloc, AllocCounterSystem};

    #[global_allocator]
    static A: AllocCounterSystem = AllocCounterSystem;

    use super::*;
    use crate::route::Route;
    use bufferpool::BufferPoolReference;
    use std::any::Any;

    struct TestRoute;

    trait AnyRoute<S: sample::Sample>: Route<S> {
        fn as_any(&self) -> &dyn Any;
    }

    type S = f32;
    type C = ();
    type R = Box<dyn AnyRoute<S, Context = ()>>;
    type N = Node<S, R>;

    impl Route<S> for TestRoute {
        type Context = ();

        fn process(
            &mut self,
            input: &[BufferPoolReference<S>],
            output: &mut [BufferPoolReference<S>],
            _frames: usize,
            _context: &mut Self::Context,
        ) {
            for (a, b) in output.iter_mut().zip(input.iter()) {
                for (output, input) in a.as_mut().iter_mut().zip(b.as_ref().iter()) {
                    *output = *input;
                }
            }
        }
    }

    impl AnyRoute<S> for TestRoute {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct InputRoute {
        input: Vec<S>,
    }

    impl Route<S> for InputRoute {
        type Context = ();

        fn process(
            &mut self,
            _input: &[BufferPoolReference<S>],
            output: &mut [BufferPoolReference<S>],
            _frames: usize,
            _context: &mut Self::Context,
        ) {
            for stream in output.iter_mut() {
                for (output, input) in stream.as_mut().iter_mut().zip(self.input.iter()) {
                    *output = *input;
                }
            }
        }
    }

    impl AnyRoute<S> for InputRoute {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct OutputRoute {
        output: Vec<S>,
        position: usize,
    }

    impl Route<S> for OutputRoute {
        type Context = ();

        fn process(
            &mut self,
            input: &[BufferPoolReference<S>],
            _output: &mut [BufferPoolReference<S>],
            frames: usize,
            _context: &mut Self::Context,
        ) {
            let len = self.output.len();
            let position = self.position;

            let mut new_position = 0;

            for stream in input.iter() {
                for (pos, input) in (0..len)
                    .cycle()
                    .skip(position)
                    .zip(stream.as_ref().iter())
                    .take(frames)
                {
                    self.output[pos] = *input;
                    new_position = pos + 1;
                }
            }

            self.position = new_position;
        }
    }

    impl AnyRoute<S> for OutputRoute {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct CountingNode {
        current: usize,
    }

    impl Route<S> for CountingNode {
        type Context = ();

        fn process(
            &mut self,
            _input: &[BufferPoolReference<S>],
            output: &mut [BufferPoolReference<S>],
            frames: usize,
            _context: &mut Self::Context,
        ) {
            for sample in output[0].as_mut().iter_mut().take(frames) {
                *sample = self.current as f32;
                self.current += 1;
            }
        }
    }

    impl AnyRoute<S> for CountingNode {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    impl AnyRoute<S> for Box<dyn AnyRoute<S, Context = ()>> {
        fn as_any(&self) -> &dyn Any {
            (**self).as_any()
        }
    }

    impl Route<S> for Box<dyn AnyRoute<S, Context = ()>> {
        type Context = ();

        fn process(
            &mut self,
            input: &[BufferPoolReference<S>],
            output: &mut [BufferPoolReference<S>],
            frames: usize,
            context: &mut C,
        ) {
            (**self).process(input, output, frames, context);
        }
    }

    fn create_node(id: Index, mut connections: Vec<Index>) -> N {
        Node::with_id(
            id,
            1,
            Box::new(TestRoute),
            connections
                .drain(..)
                .map(|id| Connection::new(id, 1.))
                .collect::<Vec<Connection<S>>>(),
        )
    }

    #[test]
    fn test_multiple_outs_signal_flow() {
        let mut graph: RouteGraph<S, R> = RouteGraphBuilder::new().with_buffer_size(32).build();

        let output = graph.add_node_with_idx(|id| {
            Node::with_id(
                id,
                1,
                Box::new(OutputRoute {
                    output: vec![0.; 32],
                    position: 0,
                }),
                vec![],
            )
        });

        let a = graph.add_node_with_idx(|id| create_node(id, vec![output]));
        let b = graph.add_node_with_idx(|id| create_node(id, vec![output]));
        let c = graph.add_node_with_idx(|id| create_node(id, vec![output]));

        graph.add_node_with_idx(|id| {
            Node::with_id(
                id,
                1,
                Box::new(InputRoute {
                    input: vec![0.5; 32],
                }),
                vec![
                    Connection::new(a, 1.),
                    Connection::new(b, 0.5),
                    Connection::new(c, 0.5),
                ],
            )
        });

        graph.topographic_sort();

        assert_eq!(graph.has_cycles(), false);

        let mut c = ();

        deny_alloc(|| {
            graph.process(32, &mut c);
        });

        let output = graph
            .with_node_mut(output, |node| {
                node.route()
                    .as_any()
                    .downcast_ref::<OutputRoute>()
                    .unwrap()
                    .output
                    .clone()
            })
            .unwrap();

        assert_eq!(output, vec![1.; 32]);
    }

    #[test]
    fn test_signal_flow() {
        let mut graph: RouteGraph<S, R> = RouteGraphBuilder::new().with_buffer_size(32).build();

        let output = graph.add_node_with_idx(|id| {
            Node::with_id(
                id,
                1,
                Box::new(OutputRoute {
                    output: vec![0.; 32],
                    position: 0,
                }),
                vec![],
            )
        });

        let a = graph.add_node_with_idx(|id| create_node(id, vec![output.clone()]));
        let b = graph.add_node_with_idx(|id| create_node(id, vec![output.clone()]));

        graph.add_node_with_idx(|id| {
            Node::with_id(
                id,
                1,
                Box::new(InputRoute {
                    input: vec![1.; 32],
                }),
                vec![
                    Connection::new(a.clone(), 0.5),
                    Connection::new(b.clone(), 0.5),
                ],
            )
        });

        graph.topographic_sort();

        assert_eq!(graph.has_cycles(), false);

        let mut c = ();

        deny_alloc(|| {
            graph.process(32, &mut c);
        });

        let output = graph
            .with_node_mut(output, |node| {
                node.route()
                    .as_any()
                    .downcast_ref::<OutputRoute>()
                    .unwrap()
                    .output
                    .clone()
            })
            .unwrap();

        assert_eq!(output, vec![1.; 32]);
    }

    #[test]
    fn test_signal_flow_counting() {
        let mut graph: RouteGraph<S, R> = RouteGraphBuilder::new().with_buffer_size(32).build();

        let output = graph.add_node_with_idx(|id| {
            Node::with_id(
                id,
                1,
                Box::new(OutputRoute {
                    output: vec![0.; 1024],
                    position: 0,
                }),
                vec![],
            )
        });

        graph.add_node_with_idx(|id| {
            Node::with_id(
                id,
                1,
                Box::new(CountingNode { current: 0 }),
                vec![Connection::new(output.clone(), 1.)],
            )
        });

        let mut c = ();

        deny_alloc(|| {
            graph.process(1024, &mut c);
        });

        let mut test: Vec<f32> = vec![0.; 1024];
        for (index, value) in test.iter_mut().enumerate() {
            *value = index as f32;
        }

        let output = graph
            .with_node_mut(output, |node| {
                node.route()
                    .as_any()
                    .downcast_ref::<OutputRoute>()
                    .unwrap()
                    .output
                    .clone()
            })
            .unwrap();

        assert_eq!(output, test);
    }

    #[test]
    fn test_simple_topo_sort() {
        let mut graph: RouteGraph<S, R> = RouteGraphBuilder::new().with_buffer_size(32).build();

        let b = graph.add_node_with_idx(|id| create_node(id, vec![]));
        let a = graph.add_node_with_idx(|id| create_node(id, vec![b]));

        assert!(graph.has_cycles());

        assert_eq!(graph.ordering.clone(), vec![b.clone(), a.clone()]);

        graph.topographic_sort();

        assert_eq!(graph.has_cycles(), false);

        assert_eq!(graph.ordering.clone(), vec![a.clone(), b.clone()]);
    }

    #[test]
    fn test_long_line_topo_sort() {
        let mut graph: RouteGraph<S, R> = RouteGraphBuilder::new().with_buffer_size(32).build();

        let f = graph.add_node_with_idx(|id| create_node(id, vec![]));
        let e = graph.add_node_with_idx(|id| create_node(id, vec![f]));
        let d = graph.add_node_with_idx(|id| create_node(id, vec![e]));
        let c = graph.add_node_with_idx(|id| create_node(id, vec![d]));
        let b = graph.add_node_with_idx(|id| create_node(id, vec![c]));
        let a = graph.add_node_with_idx(|id| create_node(id, vec![b]));

        assert_eq!(graph.has_cycles(), true);
        assert_eq!(graph.ordering.clone(), vec![f, e, d, c, b, a]);

        graph.topographic_sort();

        assert_eq!(graph.has_cycles(), false);
        assert_eq!(graph.ordering.clone(), vec![a, b, c, d, e, f,]);
    }
}
