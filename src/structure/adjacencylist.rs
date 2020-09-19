//! Logic for storing, loading and using adjacency lists.
//!
//! An adjacency list conceptually stores pairs of u64. the numbers on
//! the left-hand-side of this pair form a continuous range from 1 up
//! to some maximum, while the right-hand-side can be anything.
//!
//! Internally, this is stored as a `LogArray` and a `BitIndex` of equal length, where the
//! LogArray stores all the right-hand-sides, while the BitIndex
//! stores the boundaries between left-hand-sides (storing a 0 if this
//! left-hand-side has more pairs to follow, or 1 if this was the last
//! pair).

use std::convert::TryInto;

use bytes::Bytes;
use tokio::prelude::*;
use futures::prelude::*;
use futures::task::Poll;

use super::bitarray::*;
use super::bitindex::*;
use super::logarray::*;
use crate::storage::*;

#[derive(Clone)]
pub struct AdjacencyList {
    pub nums: LogArray,
    pub bits: BitIndex,
}

impl AdjacencyList {
    pub fn from_parts(nums: LogArray, bits: BitIndex) -> AdjacencyList {
        debug_assert_eq!(nums.len(), bits.len());
        AdjacencyList { nums, bits }
    }

    pub fn parse(
        nums_slice: Bytes,
        bits_slice: Bytes,
        bits_block_slice: Bytes,
        bits_sblock_slice: Bytes,
    ) -> AdjacencyList {
        let nums = LogArray::parse(nums_slice).unwrap();
        let bit_array = BitArray::from_bits(bits_slice).unwrap();
        let bits_block_array = LogArray::parse(bits_block_slice).unwrap();
        let bits_sblock_array = LogArray::parse(bits_sblock_slice).unwrap();
        let bits = BitIndex::from_parts(bit_array, bits_block_array, bits_sblock_array);

        Self::from_parts(nums, bits)
    }

    pub fn left_count(&self) -> usize {
        if self.bits.len() == 0 {
            0
        } else {
            self.bits.rank1((self.bits.len() as u64) - 1) as usize
        }
    }

    pub fn right_count(&self) -> usize {
        self.bits.len()
    }

    pub fn offset_for(&self, index: u64) -> u64 {
        if index == 1 {
            0
        } else {
            self.bits.select1(index - 1).unwrap() + 1
        }
    }

    pub fn pair_at_pos(&self, pos: u64) -> (u64, u64) {
        let left = if pos == 0 {
            0
        } else {
            self.bits.rank1(pos - 1)
        } + 1;
        let right = self.nums.entry(pos as usize);

        (left, right)
    }

    pub fn left_at_pos(&self, pos: u64) -> u64 {
        if pos == 0 {
            1
        } else {
            self.bits.rank1(pos - 1) + 1
        }
    }

    pub fn bit_at_pos(&self, pos: u64) -> bool {
        self.bits.get(pos)
    }

    pub fn num_at_pos(&self, pos: u64) -> u64 {
        self.nums.entry(pos.try_into().unwrap())
    }

    pub fn get(&self, index: u64) -> LogArray {
        if index < 1 {
            panic!("minimum index has to be 1");
        }
        if index > self.left_count() as u64 {
            panic!(
                "index {} too large for adjacency list of length {}",
                index,
                self.left_count()
            );
        }

        let start = self.offset_for(index);
        let end = self.bits.select1(index).unwrap();
        let length = end - start + 1;

        self.nums.slice(start as usize, length as usize)
    }

    pub fn iter(&self) -> AdjacencyListIterator {
        AdjacencyListIterator {
            pos: 0,
            left: 1,
            bits: self.bits.clone(),
            nums: self.nums.clone(),
        }
    }

    pub fn bits(&self) -> &BitIndex {
        &self.bits
    }

    pub fn nums(&self) -> &LogArray {
        &self.nums
    }
}

pub struct AdjacencyListIterator {
    pos: usize,
    left: u64,
    bits: BitIndex,
    nums: LogArray,
}

impl Iterator for AdjacencyListIterator {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        loop {
            if self.pos >= self.bits.len() {
                return None;
            }

            let bit = self.bits.get(self.pos as u64);
            let num = self.nums.entry(self.pos);

            let result = (self.left, num);
            if bit {
                self.left += 1;
            }

            self.pos += 1;

            if num == 0 {
                continue;
            }

            return Some(result);
        }
    }
}

pub struct AdjacencyBitCountStream<S: Stream<Item = Result<bool, std::io::Error>>> {
    stream: S,
    count: u64,
}

impl<S: Stream<Item = Result<bool, std::io::Error>>> AdjacencyBitCountStream<S> {
    fn new(stream: S, offset: u64) -> Self {
        AdjacencyBitCountStream {
            stream,
            count: offset,
        }
    }
}

impl<S: Stream<Item = Result<bool, std::io::Error>>> Stream for AdjacencyBitCountStream<S> {
    type Item = Result<u64, std::io::Error>;

    fn poll_next(&mut self) -> Poll<Result<u64, std::io::Error>> {
        match self.stream.poll() {
            Ok(Poll::Ready(Some(b))) => {
                let result = self.count;

                if b {
                    self.count += 1;
                }

                Ok(Poll::Ready(Some(result)))
            }
            Ok(Poll::Ready(None)) => Ok(Poll::Ready(None)),
            Ok(Poll::Pending) => Ok(Poll::Pending),
            Err(e) => Err(e),
        }
    }
}

pub fn adjacency_list_stream_pairs<F: FileLoad>(
    bits_file: F,
    nums_file: F,
) -> impl Stream<Item = Result<(u64, u64), std::io::Error>> {
    AdjacencyBitCountStream::new(bitarray_stream_bits(bits_file), 1)
        .zip(logarray_stream_entries(nums_file))
        .filter(|(_, right)| *right != 0)
}

pub struct AdjacencyListBuilder<F, W1, W2, W3>
where
    F: 'static + FileLoad + FileStore,
    W1: 'static + tokio::io::AsyncWrite + Send,
    W2: 'static + tokio::io::AsyncWrite + Send,
    W3: 'static + tokio::io::AsyncWrite + Send,
{
    bitfile: F,
    bitarray: BitArrayFileBuilder<F::Write>,
    bitindex_blocks: W1,
    bitindex_sblocks: W2,
    nums: LogArrayFileBuilder<W3>,
    last_left: u64,
    last_right: u64,
}

impl<F, W1, W2, W3> AdjacencyListBuilder<F, W1, W2, W3>
where
    F: 'static + FileLoad + FileStore,
    W1: 'static + tokio::io::AsyncWrite + Send,
    W2: 'static + tokio::io::AsyncWrite + Send,
    W3: 'static + tokio::io::AsyncWrite + Send,
{
    pub fn new(
        bitfile: F,
        bitindex_blocks: W1,
        bitindex_sblocks: W2,
        nums_writer: W3,
        width: u8,
    ) -> AdjacencyListBuilder<F, W1, W2, W3> {
        let bitarray = BitArrayFileBuilder::new(bitfile.open_write());

        let nums = LogArrayFileBuilder::new(nums_writer, width);

        AdjacencyListBuilder {
            bitfile,
            bitarray,
            bitindex_blocks,
            bitindex_sblocks,
            nums,
            last_left: 0,
            last_right: 0,
        }
    }

    pub fn push(self, left: u64, right: u64) -> impl Future<Output = Result<Self, std::io::Error>> {
        // the tricky thing with this code is that the bitarray lags one entry behind the logarray.
        // The reason for this is that at push time, we do not yet know if this entry is going to be
        // the last entry for `left`, we only know this when we push a greater `left` later on.
        let AdjacencyListBuilder {
            bitfile,
            bitarray,
            bitindex_blocks,
            bitindex_sblocks,
            nums,
            last_left,
            last_right,
        } = self;

        if left < self.last_left || (left == last_left && right <= last_right) {
            panic!("tried to push an unordered adjacent pair");
        }

        // the left hand side of the adjacencylist is expected to be a continuous range from 1 up to the max
        // but when adding entries, there may be holes. We handle holes by writing a '0' to the logarray
        // (which is otherwise an invalid right-hand side) and pushing a 1 onto the bitarray to immediately close the segment.
        let skip = left - self.last_left;

        let f1: Box<
            dyn Future<
                    Output = Result<(BitArrayFileBuilder<F::Write>, LogArrayFileBuilder<W3>),
                    std::io::Error>,
                > + Send,
        > = if last_left == 0 && skip == 1 {
            // this is the first entry. we can't push a bit yet
            Box::new(future::ok((bitarray, nums)))
        } else if skip == 0 {
            // same `left` as before. so the previous entry was not the last one, and the bitarray gets a 0 appended.
            Box::new(bitarray.push(false).map(move |bitarray| (bitarray, nums)))
        } else {
            // if this is the first element, but we do need to skip, make sure we write one less bit than we'd usually do
            let bitskip = if last_left == 0 { skip - 1 } else { skip };
            // there's a different `left`. we push a bunch of 1s to the bitarray, and 0s to the num array.
            Box::new(
                bitarray
                    .push_all(stream::iter_ok((0..bitskip).map(|_| true)))
                    .and_then(move |bitarray| {
                        nums.push_all(stream::iter_ok(0..skip - 1).map(|_| 0))
                            .map(move |nums| (bitarray, nums))
                    }),
            )
        };

        // finally push right to the logarray
        f1.and_then(move |(bitarray, nums)| {
            nums.push(right).map(move |nums| AdjacencyListBuilder {
                bitfile,
                bitarray,
                bitindex_blocks,
                bitindex_sblocks,
                nums,
                last_left: left,
                last_right: right,
            })
        })
    }

    pub fn push_all<S: Stream<Item = Result<(u64, u64), std::io::Error>>>(
        self,
        stream: S,
    ) -> impl Future<Output = Result<Self, std::io::Error>> {
        stream.fold(self, |x, (left, right)| x.push(left, right))
    }

    pub fn finalize(self) -> impl Future<Output = Result<(), std::io::Error>> {
        let AdjacencyListBuilder {
            bitfile,
            bitarray,
            bitindex_blocks,
            bitindex_sblocks,
            nums,
            last_left: _,
            last_right: _,
        } = self;
        let fut: Box<dyn Future<Output = Result<BitArrayFileBuilder<_>, std::io::Error>> + Send> =
            if nums.count() == 0 {
                Box::new(future::ok(bitarray))
            } else {
                Box::new(bitarray.push(true))
            };
        fut.and_then(|b| b.finalize())
            .and_then(|_| nums.finalize())
            .and_then(move |_| {
                build_bitindex(bitfile.open_read(), bitindex_blocks, bitindex_sblocks)
            })
            .map(|_| ())
    }

    pub fn count(&self) -> u64 {
        self.bitarray.count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::*;

    #[test]
    fn can_build_and_parse_adjacencylist() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        builder
            .push_all(stream::iter_ok(vec![(1, 1), (1, 3), (2, 5), (7, 4)]))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let bitfile_contents = bitfile.map().wait().unwrap();
        let bitindex_blocks_contents = bitindex_blocks_file.map().wait().unwrap();
        let bitindex_sblocks_contents = bitindex_sblocks_file.map().wait().unwrap();
        let nums_contents = nums_file.map().wait().unwrap();

        let adjacencylist = AdjacencyList::parse(
            nums_contents,
            bitfile_contents,
            bitindex_blocks_contents,
            bitindex_sblocks_contents,
        );

        let slice = adjacencylist.get(1);
        assert_eq!(2, slice.len());
        assert_eq!(1, slice.entry(0));
        assert_eq!(3, slice.entry(1));

        let slice = adjacencylist.get(2);
        assert_eq!(1, slice.len());
        assert_eq!(5, slice.entry(0));

        let slice = adjacencylist.get(3);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(4);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(5);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(6);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(7);
        assert_eq!(1, slice.len());
        assert_eq!(4, slice.entry(0));
    }

    #[test]
    fn empty_adjacencylist() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        builder
            .push_all(stream::iter_ok(Vec::new()))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let bitfile_contents = bitfile.map().wait().unwrap();
        let bitindex_blocks_contents = bitindex_blocks_file.map().wait().unwrap();
        let bitindex_sblocks_contents = bitindex_sblocks_file.map().wait().unwrap();
        let nums_contents = nums_file.map().wait().unwrap();

        let adjacencylist = AdjacencyList::parse(
            nums_contents,
            bitfile_contents,
            bitindex_blocks_contents,
            bitindex_sblocks_contents,
        );

        assert_eq!(0, adjacencylist.left_count());
    }

    #[test]
    fn adjacencylist_with_skip_at_start() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        builder
            .push_all(stream::iter_ok(vec![(3, 2), (7, 4)]))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let bitfile_contents = bitfile.map().wait().unwrap();
        let bitindex_blocks_contents = bitindex_blocks_file.map().wait().unwrap();
        let bitindex_sblocks_contents = bitindex_sblocks_file.map().wait().unwrap();
        let nums_contents = nums_file.map().wait().unwrap();

        let adjacencylist = AdjacencyList::parse(
            nums_contents,
            bitfile_contents,
            bitindex_blocks_contents,
            bitindex_sblocks_contents,
        );

        let slice = adjacencylist.get(1);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(2);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        /*
        let slice = adjacencylist.get(3);
        assert_eq!(1, slice.len());
        assert_eq!(2, slice.entry(0));
        */

        let slice = adjacencylist.get(4);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(5);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(6);
        assert_eq!(1, slice.len());
        assert_eq!(0, slice.entry(0));

        let slice = adjacencylist.get(7);
        assert_eq!(1, slice.len());
        assert_eq!(4, slice.entry(0));
    }

    #[test]
    fn iterate_over_adjacency_list() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        builder
            .push_all(stream::iter_ok(vec![(1, 1), (1, 3), (2, 5), (7, 4)]))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let bitfile_contents = bitfile.map().wait().unwrap();
        let bitindex_blocks_contents = bitindex_blocks_file.map().wait().unwrap();
        let bitindex_sblocks_contents = bitindex_sblocks_file.map().wait().unwrap();
        let nums_contents = nums_file.map().wait().unwrap();

        let adjacencylist = AdjacencyList::parse(
            nums_contents,
            bitfile_contents,
            bitindex_blocks_contents,
            bitindex_sblocks_contents,
        );

        assert_eq!(
            vec![(1, 1), (1, 3), (2, 5), (7, 4)],
            adjacencylist.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn iterate_over_adjacency_list_files() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        let pairs = vec![(1, 1), (1, 3), (2, 5), (7, 4)];
        builder
            .push_all(stream::iter_ok(pairs.clone()))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let result = adjacency_list_stream_pairs(bitfile, nums_file)
            .collect()
            .wait()
            .unwrap();

        assert_eq!(result, pairs);
    }

    #[test]
    fn pair_at_pos_starting_at_1_returns_correct_pair() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        let contents = vec![
            (1, 1),
            (2, 3),
            (2, 4),
            (2, 6),
            (3, 1),
            (3, 3),
            (3, 4),
            (3, 8),
            (7, 4),
            (8, 12),
            (11, 3),
        ];
        builder
            .push_all(stream::iter_ok(contents))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let bitfile_contents = bitfile.map().wait().unwrap();
        let bitindex_blocks_contents = bitindex_blocks_file.map().wait().unwrap();
        let bitindex_sblocks_contents = bitindex_sblocks_file.map().wait().unwrap();
        let nums_contents = nums_file.map().wait().unwrap();

        let adjacencylist = AdjacencyList::parse(
            nums_contents,
            bitfile_contents,
            bitindex_blocks_contents,
            bitindex_sblocks_contents,
        );

        let result: Vec<_> = (0..adjacencylist.right_count())
            .map(|i| adjacencylist.pair_at_pos(i as u64))
            .collect();

        assert_eq!(
            vec![
                (1, 1),
                (2, 3),
                (2, 4),
                (2, 6),
                (3, 1),
                (3, 3),
                (3, 4),
                (3, 8),
                (4, 0),
                (5, 0),
                (6, 0),
                (7, 4),
                (8, 12),
                (9, 0),
                (10, 0),
                (11, 3)
            ],
            result
        );
    }

    #[test]
    fn pair_at_pos_with_skip_returns_correct_pair() {
        let bitfile = MemoryBackedStore::new();
        let bitindex_blocks_file = MemoryBackedStore::new();
        let bitindex_sblocks_file = MemoryBackedStore::new();
        let nums_file = MemoryBackedStore::new();

        let builder = AdjacencyListBuilder::new(
            bitfile.clone(),
            bitindex_blocks_file.open_write(),
            bitindex_sblocks_file.open_write(),
            nums_file.open_write(),
            8,
        );
        let contents = vec![
            (2, 3),
            (2, 4),
            (2, 6),
            (3, 1),
            (3, 3),
            (3, 4),
            (3, 8),
            (7, 4),
            (8, 12),
            (11, 3),
        ];
        builder
            .push_all(stream::iter_ok(contents))
            .and_then(|b| b.finalize())
            .wait()
            .unwrap();

        let bitfile_contents = bitfile.map().wait().unwrap();
        let bitindex_blocks_contents = bitindex_blocks_file.map().wait().unwrap();
        let bitindex_sblocks_contents = bitindex_sblocks_file.map().wait().unwrap();
        let nums_contents = nums_file.map().wait().unwrap();

        let adjacencylist = AdjacencyList::parse(
            nums_contents,
            bitfile_contents,
            bitindex_blocks_contents,
            bitindex_sblocks_contents,
        );

        let result: Vec<_> = (0..adjacencylist.right_count())
            .map(|i| adjacencylist.pair_at_pos(i as u64))
            .collect();

        assert_eq!(
            vec![
                (1, 0),
                (2, 3),
                (2, 4),
                (2, 6),
                (3, 1),
                (3, 3),
                (3, 4),
                (3, 8),
                (4, 0),
                (5, 0),
                (6, 0),
                (7, 4),
                (8, 12),
                (9, 0),
                (10, 0),
                (11, 3)
            ],
            result
        );
    }
}
