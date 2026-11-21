// Copyright (c) 2023-2026 ParadeDB, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use crate::query::proximity::ProximityDistance;
use std::cmp::Ordering;
use tantivy::fieldnorm::FieldNormReader;
use tantivy::postings::Postings;
use tantivy::query::{Bm25Weight, Intersection, Scorer, SimpleUnion};
use tantivy::{DocId, DocSet, Score, TERMINATED};

/// First..last token of one side of a match (a term is a span of one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    fn at(pos: u32) -> Self {
        Span {
            start: pos,
            end: pos,
        }
    }

    pub fn cover(self, other: Span) -> Self {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// An inner clause's matches: sorted, deduplicated spans per doc, in doc id order.
#[derive(Default)]
pub struct InnerMatches {
    pub doc_ids: Vec<DocId>,
    pub offsets: Vec<u32>,
    pub spans: Vec<Span>,
}

impl InnerMatches {
    fn spans_for(&self, doc: DocId) -> &[Span] {
        match self.doc_ids.binary_search(&doc) {
            Ok(i) => &self.spans[self.offsets[i] as usize..self.offsets[i + 1] as usize],
            Err(_) => &[],
        }
    }
}

/// One side of a clause; `inner` holds the spans when the side is an inner proximity clause.
pub struct ProxSide {
    pub postings: Vec<Box<dyn Postings>>,
    pub inner: Option<InnerMatches>,
}

impl ProxSide {
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty() || self.inner.as_ref().is_some_and(|m| m.doc_ids.is_empty())
    }
}

pub struct ProximityScorer {
    #[allow(clippy::type_complexity)]
    intersection: Intersection<SimpleUnion<Box<dyn Postings>>, SimpleUnion<Box<dyn Postings>>>,
    distance: ProximityDistance,
    fieldnorm_reader: FieldNormReader,
    weight_opt: Option<Bm25Weight>,
    nmatches: usize,
    lpos: Vec<u32>,
    rpos: Vec<u32>,
    left_inner: Option<InnerMatches>,
    right_inner: Option<InnerMatches>,
    all_pairs: bool,
    lspans: Vec<Span>,
    rspans: Vec<Span>,
    span_matches: Vec<(Span, Span)>,
}

impl ProximityScorer {
    /// `all_pairs` returns every matching pair: an inner clause needs each one as a span.
    pub fn new(
        left: ProxSide,
        distance: ProximityDistance,
        right: ProxSide,
        fieldnorm_reader: FieldNormReader,
        weight_opt: Option<Bm25Weight>,
        num_docs: u32,
        all_pairs: bool,
    ) -> Self {
        let all_pairs = all_pairs || left.inner.is_some() || right.inner.is_some();
        let left_inner = left.inner;
        let right_inner = right.inner;
        let left = SimpleUnion::build(left.postings);
        let right = SimpleUnion::build(right.postings);
        let intersection = Intersection::with_two_sets(left, right, num_docs);
        let mut scorer = Self {
            intersection,
            distance,
            fieldnorm_reader,
            weight_opt,
            nmatches: 0,
            lpos: Default::default(),
            rpos: Default::default(),
            left_inner,
            right_inner,
            all_pairs,
            lspans: Default::default(),
            rspans: Default::default(),
            span_matches: Default::default(),
        };

        if scorer.doc() != TERMINATED && !scorer.prox_match() {
            scorer.advance();
        }

        scorer
    }

    pub fn fieldnorm_id(&self) -> u8 {
        self.intersection
            .docset_specialized(0)
            .fieldnorm_id()
            .unwrap_or_else(|| self.fieldnorm_reader.fieldnorm_id(self.doc()))
    }

    pub fn prox_iter(&mut self) -> impl Iterator<Item = (Span, Span)> + '_ {
        if !self.all_pairs {
            self.lpos.clear();
            self.rpos.clear();
            self.intersection
                .docset_mut_specialized(0)
                .positions(&mut self.lpos);
            self.intersection
                .docset_mut_specialized(1)
                .positions(&mut self.rpos);
            return MatchIter::Positions(ProxIter::new(self.distance, &self.lpos, &self.rpos));
        }

        let doc = self.doc();
        fill_spans(
            &mut self.lspans,
            &mut self.lpos,
            self.left_inner.as_ref(),
            self.intersection.docset_mut_specialized(0),
            doc,
        );
        fill_spans(
            &mut self.rspans,
            &mut self.rpos,
            self.right_inner.as_ref(),
            self.intersection.docset_mut_specialized(1),
            doc,
        );
        match_spans(
            self.distance,
            &self.lspans,
            &self.rspans,
            &mut self.span_matches,
        );
        MatchIter::Spans(self.span_matches.iter())
    }

    pub fn prox_count(&mut self) -> usize {
        self.prox_iter().count()
    }

    fn prox_match(&mut self) -> bool {
        let has_weight = self.weight_opt.is_some();
        let mut iter = self.prox_iter();
        if has_weight {
            let count = iter.count();
            self.nmatches = count;
            count > 0
        } else {
            iter.next().is_some()
        }
    }
}

fn fill_spans(
    spans: &mut Vec<Span>,
    positions: &mut Vec<u32>,
    inner: Option<&InnerMatches>,
    postings: &mut SimpleUnion<Box<dyn Postings>>,
    doc: DocId,
) {
    spans.clear();
    match inner {
        Some(inner) => spans.extend_from_slice(inner.spans_for(doc)),
        None => {
            positions.clear();
            postings.positions(positions);
            spans.extend(positions.iter().map(|&pos| Span::at(pos)));
        }
    }
}

/// Every (left, right) pair within `distance`; `right` is sorted by start.
fn match_spans(
    distance: ProximityDistance,
    left: &[Span],
    right: &[Span],
    out: &mut Vec<(Span, Span)>,
) {
    out.clear();
    let max_gap = distance.distance().saturating_add(1);
    let widest_right = right.iter().map(|r| r.end - r.start).max().unwrap_or(0);
    for &l in left {
        let lo = l.start.saturating_sub(max_gap.saturating_add(widest_right));
        let hi = l.end.saturating_add(max_gap);
        let first = right.partition_point(|r| r.start < lo);
        for &r in right[first..].iter().take_while(|r| r.start <= hi) {
            if distance.span_gap(l, r).is_some_and(|gap| gap <= max_gap) {
                out.push((l, r));
            }
        }
    }
}

enum MatchIter<'a> {
    Positions(ProxIter<'a>),
    Spans(std::slice::Iter<'a, (Span, Span)>),
}

impl Iterator for MatchIter<'_> {
    type Item = (Span, Span);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            MatchIter::Positions(iter) => iter.next().map(|(l, r)| (Span::at(l), Span::at(r))),
            MatchIter::Spans(iter) => iter.next().copied(),
        }
    }
}

struct ProxIter<'a> {
    distance: ProximityDistance,
    lpos: &'a [u32],
    rpos: &'a [u32],
    li: usize,
    ri: usize,
}

impl<'a> ProxIter<'a> {
    fn new(distance: ProximityDistance, lpos: &'a [u32], rpos: &'a [u32]) -> Self {
        Self {
            distance,
            lpos,
            rpos,
            li: 0,
            ri: 0,
        }
    }
}

impl Iterator for ProxIter<'_> {
    type Item = (u32, u32);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.li >= self.lpos.len() || self.ri >= self.rpos.len() {
                return None;
            }

            let l = self.lpos[self.li];
            let r = self.rpos[self.ri];

            let diff = self.distance.diff(l, r);
            if diff <= self.distance.distance() + 1 {
                self.li += 1;
                return Some((l, r));
            }

            match l.cmp(&r) {
                Ordering::Less => self.li += 1,
                Ordering::Equal => {
                    self.li += 1;
                    self.ri += 1;
                }
                Ordering::Greater => self.ri += 1,
            }
        }
    }
}

impl DocSet for ProximityScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.intersection.advance();
            if doc == TERMINATED || self.prox_match() {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        let doc = self.intersection.seek(target);
        if doc == TERMINATED || self.prox_match() {
            return doc;
        }
        self.advance()
    }

    fn doc(&self) -> DocId {
        self.intersection.doc()
    }

    fn size_hint(&self) -> u32 {
        self.intersection.size_hint()
    }
}

impl Scorer for ProximityScorer {
    fn score(&mut self) -> Score {
        if let Some(similarity_weight) = self.weight_opt.as_ref() {
            similarity_weight.score(self.fieldnorm_id(), self.nmatches as u32)
        } else {
            1.0f32
        }
    }
}
