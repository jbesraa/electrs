use crate::bitcoin::consensus::Decodable;
use crate::bitcoin::{BlockHash, Transaction, Txid};
use crate::bitcoin_slices::{bsl, EmptyVisitor, Visit};
use anyhow::{Context, Result};
use bindex::{IndexedChain, ScriptHash};

use crate::{
    config::Config,
    daemon::Daemon,
    mempool::{FeeHistogram, Mempool},
    metrics::Metrics,
    signals::ExitFlag,
    status::{Balance, ScriptHashStatus, UnspentEntry},
};

/// Electrum protocol subscriptions' tracker
pub struct Tracker {
    index: IndexedChain,
    mempool: Mempool,
    ignore_mempool: bool,
    /// Cap on how many confirmed history entries one script hash may have.
    ///
    /// Upstream 0.12.0 parses `index_lookup_limit` into `Config` and then reads it
    /// NOWHERE (checked against the crate source: the name appears only in
    /// config.rs). So one address with a multi-million-entry history walks the whole
    /// index — hundreds of MB of reads, ~1 GB RSS — and every other client on this
    /// port starves until electrs is restarted. Threading the value into the walk
    /// makes the flag do what its own help text promises.
    lookup_limit: Option<usize>,
}

/// One page of a script hash's history.
pub(crate) struct HistoryPage {
    pub(crate) entries: Vec<HistoryPageEntry>,
    /// Hand back to continue. `None` only when the scan found no candidates at all.
    pub(crate) next_cursor: Option<u32>,
    /// Whether the index holds candidates beyond the last one examined.
    pub(crate) more: bool,
}

/// One page entry: the transaction and the block it was confirmed in.
pub(crate) struct HistoryPageEntry {
    pub(crate) txid: Txid,
    pub(crate) height: u32,
}

impl Tracker {
    pub fn new(config: &Config, metrics: Metrics) -> Result<Self> {
        let url = format!("http://{}", config.daemon_rpc_addr);
        let index = IndexedChain::open(&config.db_dir, config.network, Some(url))
            .context("failed to open index")?;
        Ok(Self {
            index,
            mempool: Mempool::new(&metrics),
            ignore_mempool: config.ignore_mempool,
            lookup_limit: config.index_lookup_limit,
        })
    }

    pub(crate) fn headers(&self) -> &bindex::Headers {
        self.index.headers()
    }

    pub(crate) fn fees_histogram(&self) -> &FeeHistogram {
        self.mempool.fees_histogram()
    }

    pub(crate) fn get_unspent(&self, status: &ScriptHashStatus) -> Vec<UnspentEntry> {
        status.get_unspent()
    }

    pub(crate) fn sync(&mut self, daemon: &Daemon, exit_flag: &ExitFlag) -> Result<bool> {
        exit_flag.poll()?;
        let stats = self.index.sync(1000)?;
        let done = stats.indexed_blocks == 0;
        if done && !self.ignore_mempool {
            self.mempool.sync(daemon, exit_flag);
            // TODO: double check tip - and retry on diff
        }
        Ok(done)
    }

    pub(crate) fn status(&self) -> Result<()> {
        Ok(())
    }

    pub(crate) fn update_scripthash_status(&self, status: &mut ScriptHashStatus) -> Result<bool> {
        let prev_statushash = status.statushash();
        status.sync(&self.index, &self.mempool, self.lookup_limit)?;
        Ok(prev_statushash != status.statushash())
    }

    pub(crate) fn get_balance(&self, status: &ScriptHashStatus) -> Balance {
        status.get_balance()
    }

    /// One page of a script hash's history, WITHOUT building the whole status.
    ///
    /// `ScriptHashStatus` accumulates every entry of a history, because its job is to
    /// answer "what is this script hash's state now" and to hash it. A page needs
    /// neither: it needs (txid, height) for a bounded window. So this seeks the index at
    /// the cursor and decodes only the entries it returns — the cost is a seek plus
    /// `limit` reads, and does not grow with the size of the history. That is what makes
    /// an address with millions of transactions browsable at all.
    ///
    /// Candidates are POST-FILTERED, because the index keys a script hash by its first
    /// 8 bytes (bindex: lookups "require post-filtering"), so a candidate can belong to a
    /// different script that shares the prefix.
    ///
    /// The cursor is the last `txnum` this function looked at, and resuming is strictly
    /// past it — see the loop for why the cursor advances over candidates and not over
    /// the entries kept.
    pub(crate) fn history_page(
        &self,
        scripthash: &ScriptHash,
        cursor: Option<u32>,
        limit: usize,
        newest_first: bool,
    ) -> Result<HistoryPage> {
        let from = match (cursor, newest_first) {
            (None, true) => u32::MAX,
            (None, false) => 0,
            (Some(c), true) => c.saturating_sub(1),
            (Some(c), false) => c.saturating_add(1),
        };

        // One candidate more than asked for, so `more` reports the index rather than a
        // guess about it.
        let candidates =
            self.index
                .script_hash_page(scripthash, from, limit.saturating_add(1), newest_first)?;
        let more = candidates.len() > limit;

        let mut entries = Vec::with_capacity(limit.min(candidates.len()));
        let mut next_cursor = None;
        for candidate in candidates.into_iter().take(limit) {
            // Advance over CANDIDATES, never over just the entries kept: a page whose
            // candidates were all filtered out must still move the caller forward, or a
            // prefix shared with another script could be paged forever.
            next_cursor = Some(candidate.txnum);

            let location = self.index.location_by_txnum(candidate.txnum);
            let tx_bytes = self.index.get_tx_bytes(&location)?;
            let tx = Transaction::consensus_decode_from_finite_reader(&mut &tx_bytes[..])?;
            if !self.touches_scripthash(&tx, scripthash)? {
                continue;
            }
            entries.push(HistoryPageEntry {
                txid: tx.compute_txid(),
                height: candidate.block_height as u32,
            });
        }
        Ok(HistoryPage {
            entries,
            next_cursor,
            more,
        })
    }

    /// Whether `tx` really touches `scripthash`, on either side.
    ///
    /// The output side is checked directly. The input side needs the PREVOUT scripts,
    /// which a transaction does not carry, so those transactions are resolved through the
    /// index — and only when the output side did not already match, so a funding
    /// transaction costs no extra lookups. A txid has its own 8-byte prefix in the index
    /// too, hence the txid check on what comes back (BIP-30 has two coinbases sharing a
    /// txid).
    fn touches_scripthash(&self, tx: &Transaction, scripthash: &ScriptHash) -> Result<bool> {
        if tx
            .output
            .iter()
            .any(|output| ScriptHash::new(&output.script_pubkey) == *scripthash)
        {
            return Ok(true);
        }

        for input in &tx.input {
            let prevout = &input.previous_output;
            if prevout.is_null() {
                continue;
            }
            for location in self.index.locations_by_txid(&prevout.txid)? {
                let bytes = self.index.get_tx_bytes(&location)?;
                let prev = Transaction::consensus_decode_from_finite_reader(&mut &bytes[..])?;
                if prev.compute_txid() != prevout.txid {
                    continue;
                }
                if let Some(output) = prev.output.get(prevout.vout as usize) {
                    if ScriptHash::new(&output.script_pubkey) == *scripthash {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    pub(crate) fn lookup_transaction(&self, txid: Txid) -> Result<Option<(BlockHash, Box<[u8]>)>> {
        // Note: there are two blocks with coinbase transactions having same txid (see BIP-30)
        for loc in self.index.locations_by_txid(&txid)? {
            let tx_bytes = self.index.get_tx_bytes(&loc)?;
            if txid == compute_txid(&tx_bytes)? {
                return Ok(Some((loc.block_hash(), tx_bytes.into_boxed_slice())));
            }
        }
        Ok(None)
    }
}

fn compute_txid(tx_bytes: &[u8]) -> Result<Txid> {
    let mut visit = EmptyVisitor {};
    let res = bsl::Transaction::visit(tx_bytes, &mut visit)
        .map_err(|err| anyhow!("invalid transaction: {:?}", err))?;
    ensure!(res.remaining().is_empty(), "non-empty remaining bytes");
    Ok(Txid::from_raw_hash(res.parsed().txid()))
}
