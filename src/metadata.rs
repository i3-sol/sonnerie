extern crate rusqlite;
extern crate byteorder;
extern crate antidote;

#[derive(Debug,Clone,Copy,PartialEq,PartialOrd)]
pub struct Timestamp(pub u64);

use ::row_format::{parse_row_format, RowFormat};
use ::db::Db;
use ::blocks::Blocks;
use self::byteorder::{ByteOrder, BigEndian};
use std::path::Path;

use std::sync::Arc;
pub use self::antidote::RwLock;
use std::cell::{Cell,RefCell};

/// Maintain all the information needed to locate data
/// One of these is opened per transaction/thread
pub struct Metadata
{
	db: rusqlite::Connection,
	blocks: Arc<RwLock<Blocks>>,
	pub next_offset: Cell<u64>,
	pub generation: u64,
}

impl Metadata
{
	/// open an existing database.
	///
	/// `next_offset` is the end of the block data where new blocks are created
	/// `f` is the filename of the existing metadata file
	/// `blocks` is shared between threads
	pub fn open(next_offset: u64, f: &Path, blocks: Arc<RwLock<Blocks>>)
		-> Metadata
	{
		let db = rusqlite::Connection::open_with_flags(
			f,
			rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
				| rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
		).unwrap();
		db.execute_batch("PRAGMA case_sensitive_like=ON;").unwrap();
		Metadata
		{
			db: db,
			next_offset: Cell::new(next_offset),
			blocks: blocks,
			generation: 1,
		}
	}

	/// open or create a metadata file.
	///
	/// This is called only once at startup
	pub fn new(next_offset: u64, f: &Path, blocks: Arc<RwLock<Blocks>>)
		-> Metadata
	{
		let db = rusqlite::Connection::open_with_flags(
			f,
			rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
				| rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
				| rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
		).unwrap();
		db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
		db.execute_batch("PRAGMA case_sensitive_like=ON;").unwrap();

		db.execute_batch(
			"
				begin;
				create table if not exists schema_version (
					-- the version of the schema (for upgrading)
					version integer primary key not null
				);

				create table if not exists series (
					-- each series gets a numeric id
					series_id integer primary key autoincrement,
					-- the string that the user refers to this series by
					name text,
					-- which transaction did this appear in
					-- (this series is not visible to transactions
					-- that predate this generation)
					generation integer,
					format text
				);

				create index if not exists series_name on series (name collate binary);
				create index if not exists series_gen on series (generation);

				-- which blocks are associated with which series
				create table if not exists series_blocks (
					series_id integer,
					-- when this block last changed (for backup)
					generation integer,
					first_timestamp integer,
					last_timestamp integer,
					offset integer,
					capacity integer,
					size integer,
					constraint series_ts primary key (series_id, first_timestamp)
				);
				commit;
			"
		).unwrap();
		Metadata
		{
			db: db,
			next_offset: Cell::new(next_offset),
			blocks: blocks,
			generation: 1,
		}
	}

	/// Called on startup to determine what generation the db is at
	pub fn last_generation(&self)
		-> u64
	{
		let g: i64 = self.db.query_row(
			"select generation from series order by generation desc limit 1",
			&[],
			|r| r.get(0)
		).unwrap_or(0);
		g as u64
	}

	/// Starts a transaction and converts me to a Transaction
	pub fn as_read_transaction(self)
		-> Transaction<'static>
	{
		self.db.execute("begin", &[]).unwrap();
		Transaction
		{
			metadata: self,
			writing: false,
			committed: false,
			finishing_on: None,
		}
	}

	/// Starts a transaction and converts me to a writable Transaction
	pub fn as_write_transaction<'db>(
		mut self,
		new_generation: u64,
		finishing_on: &'db Db,
	)
		-> Transaction<'db>
	{
		self.db.execute("begin", &[]).unwrap();
		self.generation = new_generation;
		Transaction
		{
			metadata: self,
			writing: true,
			committed: false,
			finishing_on: Some(finishing_on)
		}
	}
}

pub struct Transaction<'db>
{
	metadata: Metadata,
	writing: bool,
	committed: bool,
	finishing_on: Option<&'db Db>,
}

impl<'db> Transaction<'db>
{
	/// Gets the blocks associated with a range of timestamps
	fn blocks_for_range(
		&self,
		series_id: u64,
		first_ts: Timestamp,
		last_ts: Timestamp,
	) -> Vec<Block>
	{
		let mut s = self.metadata.db.prepare_cached("
			select
				first_timestamp,
				last_timestamp,
				offset,
				capacity,
				size
			from series_blocks
			where
				series_id=? and
				? >= first_timestamp AND last_timestamp >=  ?
		").unwrap();

		let mut rows = s.query(&[
			&(series_id as i64),
			&last_ts.to_sqlite(),
			&first_ts.to_sqlite(),
		]).unwrap();

		let mut blocks = vec!();
		while let Some(row) = rows.next()
		{
			let row = row.unwrap();
			let b = Block
			{
				first_timestamp: Timestamp::from_sqlite(row.get(0)),
				last_timestamp: Timestamp::from_sqlite(row.get(1)),
				offset: row.get::<_,i64>(2) as u64,
				capacity: row.get::<_,i64>(3) as u64,
				size: row.get::<_,i64>(4) as u64,
			};
			blocks.push( b );
		}
		blocks
	}

	fn series_format(&self, series_id: u64) -> Box<RowFormat>
	{
		let mut c = self.metadata.db.prepare_cached(
			"select format from series where series_id=?"
		).unwrap();

		let v: String = c.query(&[&(series_id as i64)]).unwrap()
			.next()
			.map(|e| e.unwrap().get(0))
			.unwrap();

		let f = parse_row_format(&v);
		f
	}

	pub fn series_format_string(&self, name: &str)
		-> Option<String>
	{
		let mut c = self.metadata.db.prepare_cached(
			"select format from series where name=?"
		).unwrap();

		let v = c.query(&[&name]).unwrap()
			.next()
			.map(|e| e.unwrap().get(0));
		v
	}

	/// creates a new series if necessary
	///
	/// Returns its ID, or None if the format doesn't match
	pub fn create_series(
		&mut self,
		name: &str,
		format: &str
	) -> Option<u64>
	{
		if !self.writing
			{ panic!("attempt to write in a read-only transaction"); }

		let mut q = self.metadata.db.prepare_cached(
			"select series_id,format from series where name=?"
		).unwrap();
		let mut row = q.query(&[&name]).unwrap();
		if let Some(row) = row.next()
		{
			let row = row.unwrap();
			let id: i64 = row.get(0);
			let stored_format: String = row.get(1);
			if stored_format != format
			{
				return None;
			}
			return Some(id as u64);
		}

		self.metadata.db.execute(
			"insert into series (name, generation, format)
				values (?, ?, ?)
			",
			&[
				&name,
				&(self.metadata.generation as i64),
				&format,
			]
		).unwrap();

		Some(self.metadata.db.last_insert_rowid() as u64)
	}

	/// Returns a series's ID
	pub fn series_id(
		&self,
		name: &str
	) -> Option<u64>
	{
		let mut c = self.metadata.db.prepare_cached(
			"select series_id from series where name=?"
		).unwrap();

		let v = c.query(&[&name]).unwrap()
			.next()
			.map(|e| e.unwrap().get::<_,i64>(0) as u64);
		v
	}

	/// return all of the series IDs that are SQL-like
	/// this string
	pub fn series_like<F>(
		&self,
		like: &str,
		mut callback: F,
	)
		where F: FnMut(&str, u64)
	{
		let mut c = self.metadata.db.prepare_cached(
			"select name, series_id from series where name like ?"
		).unwrap();
		let mut rows = c.query(&[&like]).unwrap();
		while let Some(row) = rows.next()
		{
			let row = row.unwrap();
			callback(
				&row.get::<_,String>(0),
				row.get::<_,i64>(1) as u64,
			);
		}
	}


	/// Inserts many values into a series
	///
	/// The timestamps must be sorted
	pub fn insert_into_series<Generator>(
		&mut self,
		series_id: u64,
		mut generator: Generator,
	) -> Result<(), String>
		where Generator: FnMut(&RowFormat, &mut Vec<u8>) -> Option<Timestamp>
	{
		if !self.writing
		{
			Err("attempt to write in a \
				read-only transaction".to_string())?;
		}
		let mut save = Savepoint::new(&self.metadata.db)?;
		{
			let format = self.series_format(series_id);
			let preferred_block_size = format.preferred_block_size();

			let buffer = RefCell::new(vec!());
			buffer.borrow_mut().reserve(preferred_block_size);

			let mut done = false;

			while !done
			{
				let last_block = self.last_block_for_series(series_id);

				let fits_in_block =
					if let Some(last_block) = last_block.as_ref()
					{
						(last_block.capacity-last_block.size)
							%preferred_block_size as u64
					}
					else
					{
						0
					};


				let mut fill_buffer =
					|n_bytes: usize, last_block: &Option<Block>, done: &mut bool|
					{
						let mut buffer = buffer.borrow_mut();
						buffer.clear();
						let mut first_timestamp = None;
						let mut last_timestamp = last_block.as_ref().map(
							|b| b.last_timestamp
						);
						while buffer.len() < n_bytes
						{
							let r = generator(&*format, &mut buffer);
							if r.is_none() { *done = true; break; }
							let r = r.unwrap();
							if first_timestamp.is_none()
								{ first_timestamp = Some(r); }
							if let Some(p) = last_timestamp.clone()
							{
								if r <= p
								{
									return Err(format!("timestamps must be increasing (\
										{}<={})", r.0, p.0));
								}
							}
							last_timestamp = Some(r);
						}

						if first_timestamp.is_none()
						{
							Ok(None)
						}
						else
						{
							Ok(Some((
								first_timestamp.unwrap(),
								last_timestamp.unwrap(),
							)))
						}

					};

				let new_block;


				if fits_in_block == 0
				{ // new block
					let range = fill_buffer(preferred_block_size, &last_block, &mut done)?;
					let buffer = buffer.borrow();
					if range.is_none() { break; }
					let (first_timestamp,last_timestamp) = range.unwrap();
					let mut b =
						self.create_new_block(
							series_id,
							first_timestamp,
							last_timestamp,
							buffer.len(),
							preferred_block_size,
						);
					b.size = 0;
					new_block = b;
				}
				else
				{ // fill the existing block
					let range = fill_buffer(fits_in_block as usize, &last_block, &mut done)?;
					if range.is_none() { break; }
					let (_, last_timestamp) = range.unwrap();
					let buffer = buffer.borrow();
					new_block = last_block.unwrap();
					self.resize_existing_block(
						series_id,
						new_block.first_timestamp,
						last_timestamp,
						new_block.size + buffer.len() as u64,
					);
				}

				let buffer = buffer.borrow();
				self.metadata.blocks.write()
					.write(
						new_block.offset+new_block.size,
						&buffer
					);

			}
		}
		save.commit()?;
		Ok(())
	}

	/// reads values for a range of timestamps.
	///
	/// the timestamps are inclusive
	pub fn read_series<Output>(
		&self,
		series_id: u64,
		first_timestamp: Timestamp,
		last_timestamp: Timestamp,
		mut out: Output,
	)
		where Output: FnMut(&Timestamp, &RowFormat, &[u8])
	{
		let blocks = self.blocks_for_range(
			series_id,
			first_timestamp,
			last_timestamp,
		);
		// eprintln!("blocks for range: {:?}", blocks);
		if blocks.is_empty() { return; }

		let format = self.series_format(series_id);

		let mut block_data = vec!();
		block_data.reserve(format.preferred_block_size());

		let mut done = false;

		for block in blocks
		{
			block_data.resize(block.size as usize, 0u8);
			self.metadata.blocks.read()
				.read(block.offset, &mut block_data[..]);

			for sample in block_data.chunks(format.row_size())
			{
				let t = Timestamp(BigEndian::read_u64(&sample[0..8]));
				if t >= first_timestamp
				{
					if t > last_timestamp
					{
						done = true;
						break;
					}
					out(&t, &*format, &sample[8..]);
				}
			}

			if done { break; }
		}
	}

	/// creates a block in the metadata (does not populate the block)
	///
	/// `initial_size` is its used sized, all of which must be populated.
	///
	/// `initial_size` may be larger than the default capacity (a
	/// larger capacity is used).
	fn create_new_block(
		&self,
		series_id: u64,
		first_timestamp: Timestamp,
		last_timestamp: Timestamp,
		initial_size: usize, // not capacity
		capacity: usize,
	) -> Block
	{
		let capacity = capacity.max(initial_size);

		self.metadata.db.execute(
			"insert into series_blocks (
				series_id, generation, first_timestamp,
				last_timestamp, offset,
				capacity, size
			) values (
				?,?,?,?,?,?,?
			)",
			&[
				&(series_id as i64),
				&(self.metadata.generation as i64),
				&first_timestamp.to_sqlite(),
				&last_timestamp.to_sqlite(),
				&(self.metadata.next_offset.get() as i64),
				&(capacity as i64), &(initial_size as i64),
			]
		).unwrap();
		let b = Block
		{
			first_timestamp: first_timestamp,
			last_timestamp: last_timestamp,
			offset: self.metadata.next_offset.get(),
			capacity: capacity as u64,
			size: initial_size as u64,
		};


		self.metadata.next_offset.set(
			self.metadata.next_offset.get() + capacity as u64
		);

		b
	}

	fn resize_existing_block(
		&self,
		series_id: u64,
		first_timestamp: Timestamp,
		new_last_timestamp: Timestamp,
		new_size: u64,
	)
	{
		self.metadata.db.execute(
			"update series_blocks
			set
				size=?, last_timestamp=?,
				generation=?
			where
				series_id=? and first_timestamp=?
			",
			&[
				&(new_size as i64), &new_last_timestamp.to_sqlite(),
				&(self.metadata.generation as i64),
				&(series_id as i64), &first_timestamp.to_sqlite(),
			]
		).unwrap();

	}

	fn last_block_for_series(
		&self,
		series_id: u64,
	) -> Option<Block>
	{
		let mut s = self.metadata.db.prepare_cached("
			select
				first_timestamp,
				last_timestamp,
				offset,
				capacity,
				size
			from series_blocks
			where
				series_id=?
			order by first_timestamp desc
			limit 1
		").unwrap();

		let mut rows = s.query(&[&(series_id as i64)]).unwrap();

		if let Some(row) = rows.next()
		{
			let row = row.unwrap();
			let b = Block
			{
				first_timestamp: Timestamp::from_sqlite(row.get(0)),
				last_timestamp: Timestamp::from_sqlite(row.get(1)),
				offset: row.get::<_,i64>(2) as u64,
				capacity: row.get::<_,i64>(3) as u64,
				size: row.get::<_,i64>(4) as u64,
			};
			Some(b)
		}
		else
		{
			None
		}
	}

	pub fn commit(mut self)
	{
		if self.writing
		{
			self.metadata.blocks.write().commit();
			self.finishing_on.unwrap()
				.committing(&self.metadata);
		}
		self.committed = true;
		self.metadata.db.execute("commit", &[]).unwrap();
	}
}

impl<'db> Drop for Transaction<'db>
{
	fn drop(&mut self)
	{
		if !self.committed
		{
			self.metadata.db.execute("rollback", &[]).unwrap();
		}
	}
}

struct Savepoint<'conn>
{
	conn: &'conn rusqlite::Connection,
	done: bool,
}

impl<'conn> Savepoint<'conn>
{
	fn new(conn: &'conn rusqlite::Connection)
		-> Result<Savepoint, String>
	{
		conn.execute("savepoint sp", &[])
			.map_err(|e| format!("failed to begin savepoint: {}", e))?;
		Ok(Savepoint
		{
			conn: conn,
			done: false,
		})
	}

	fn commit(&mut self) -> Result<(), String>
	{
		self.conn.execute(
			"release savepoint sp", &[]
		)
			.map_err(|e| format!("failed to release savepoint: {}", e))?;
		self.done = true;
		Ok(())
	}
}

impl<'conn> Drop for Savepoint<'conn>
{
	fn drop(&mut self)
	{
		if !self.done
		{
			let _ = self.conn.execute(
				"rollback to savepoint sp", &[]
			);
		}
	}
}

/// Map u64 to i64, because sqlite doesn't do unsigned 64-bit
///
/// We just subtract the difference so that sorting is still the same
impl Timestamp
{
	fn to_sqlite(&self) -> i64
	{
		(self.0 as i64).wrapping_add(::std::i64::MIN)
	}
	fn from_sqlite(v: i64) -> Timestamp
	{
		Timestamp(v.wrapping_sub(::std::i64::MIN) as u64)
	}
}

#[cfg(test)]
mod tests
{
	use ::metadata::Timestamp;
	#[test]
	fn timestamp_range()
	{
		assert_eq!(Timestamp(::std::u64::MAX).to_sqlite(), ::std::i64::MAX);
		assert_eq!(Timestamp(500).to_sqlite(), ::std::i64::MIN+500);
		assert_eq!(Timestamp(0).to_sqlite(), ::std::i64::MIN);

		assert_eq!(Timestamp::from_sqlite(::std::i64::MIN).0, 0);
		assert_eq!(Timestamp::from_sqlite(0).0-1, ::std::i64::MAX as u64);

		for some in &[::std::i64::MIN, ::std::i64::MIN+100, 0, 100, ::std::i64::MAX-1000]
		{
			assert_eq!(Timestamp::from_sqlite(*some).to_sqlite(), *some);
		}
	}
}

#[derive(Debug)]
struct Block
{
	first_timestamp: Timestamp,
	last_timestamp: Timestamp,
	offset: u64,
	capacity: u64,
	size: u64,
}
