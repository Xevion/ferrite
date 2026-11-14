//! Test support shared by the modules: the test binary's allocator, counting the heap allocations
//! (and reallocations) of each thread.

pub use ferrite_testing::counting::count;

#[global_allocator]
static ALLOCATOR: ferrite_testing::counting::Counting = ferrite_testing::counting::Counting;
