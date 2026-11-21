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

use crate::query::proximity::query::ProximityQuery;
use crate::query::proximity::scorer::{InnerMatches, ProxSide, ProximityScorer};
use crate::query::proximity::{ProxTermStyle, ProximityClause, WhichTerms};
use std::sync::Arc;
use tantivy::fieldnorm::FieldNormReader;
use tantivy::postings::{LoadedPostings, Postings};
use tantivy::query::{
    AutomatonWeight, Bm25Weight, EmptyScorer, Explanation, RegexPhraseWeight, Scorer, Weight,
    does_not_match,
};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocId, DocSet, Score, SegmentReader, TERMINATED, Term};

pub struct ProximityWeight {
    query: ProximityQuery,
    weight_opt: Option<Bm25Weight>,
}

impl ProximityWeight {
    pub fn new(query: ProximityQuery, bm25_weight_opt: Option<Bm25Weight>) -> Self {
        Self {
            query,
            weight_opt: bm25_weight_opt,
        }
    }

    fn fieldnorm_reader(&self, reader: &SegmentReader) -> tantivy::Result<FieldNormReader> {
        let field = self.query.field();
        if self.weight_opt.is_some() {
            return reader.scoring_fieldnorm_reader(field);
        }
        Ok(FieldNormReader::constant(reader.max_doc(), 1))
    }

    fn prox_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> tantivy::Result<Option<ProximityScorer>> {
        let weight_opt = self
            .weight_opt
            .as_ref()
            .map(|bm25_weight| bm25_weight.boost_by(boost));
        let fieldnorm_reader = self.fieldnorm_reader(reader)?;

        let left = self.read_side(reader, self.query.left(), WhichTerms::Left)?;
        if left.is_empty() {
            return Ok(None);
        }
        let right = self.read_side(reader, self.query.right(), WhichTerms::Right)?;
        if right.is_empty() {
            return Ok(None);
        }

        Ok(Some(ProximityScorer::new(
            left,
            self.query.distance(),
            right,
            fieldnorm_reader,
            weight_opt,
            reader.max_doc(),
            false,
        )))
    }

    fn read_side(
        &self,
        segment_reader: &SegmentReader,
        clause: &ProximityClause,
        which_terms: WhichTerms,
    ) -> tantivy::Result<ProxSide> {
        if let ProximityClause::Proximity {
            left,
            distance,
            right,
        } = clause
        {
            let left = self.read_side(segment_reader, left, which_terms)?;
            let right = self.read_side(segment_reader, right, which_terms)?;
            let mut scorer = ProximityScorer::new(
                left,
                *distance,
                right,
                self.fieldnorm_reader(segment_reader)?,
                self.weight_opt.clone(),
                segment_reader.max_doc(),
                true,
            );

            let mut matches = InnerMatches::default();
            let mut fieldnorms = Vec::new();
            let mut doc_spans = Vec::new();
            while scorer.doc() != TERMINATED {
                matches.offsets.push(matches.spans.len() as u32);
                matches.doc_ids.push(scorer.doc());
                if self.weight_opt.is_some() {
                    fieldnorms.push(scorer.fieldnorm_id());
                }

                doc_spans.clear();
                doc_spans.extend(scorer.prox_iter().map(|(l, r)| l.cover(r)));
                doc_spans.sort_unstable();
                doc_spans.dedup();
                matches.spans.extend_from_slice(&doc_spans);
                scorer.advance();
            }
            matches.offsets.push(matches.spans.len() as u32);

            let postings = LoadedPostings {
                doc_ids: matches.doc_ids.clone().into_boxed_slice(),
                position_offsets: vec![0; matches.doc_ids.len() + 1].into_boxed_slice(),
                positions: Box::default(),
                cursor: 0,
                fieldnorms: self
                    .weight_opt
                    .as_ref()
                    .map(|_| fieldnorms.into_boxed_slice()),
            };

            Ok(ProxSide {
                postings: vec![Box::new(postings)],
                inner: Some(matches),
            })
        } else {
            let mut postings: Vec<Box<dyn Postings>> = Vec::new();
            let mut num_regex_terms = 0;
            let inverted_index = segment_reader.inverted_index(self.query.field())?;
            for term in clause.terms(self.query.field(), Some(segment_reader), which_terms)? {
                match term {
                    ProxTermStyle::Term(term) => {
                        let term = Term::from_field_text(self.query.field(), term.as_ref());
                        if let Some(segment_postings) = inverted_index
                            .read_postings(&term, IndexRecordOption::WithFreqsAndPositions)?
                        {
                            postings.push(Box::new(segment_postings));
                        }
                    }
                    ProxTermStyle::Regex(re, max_expansions) => {
                        let regex =
                            tantivy_fst::Regex::new(re.as_str()).unwrap_or_else(|e| panic!("{e}"));
                        let automaton = AutomatonWeight::<tantivy_fst::Regex>::new(
                            self.query.field(),
                            Arc::new(regex),
                        );
                        let term_infos = automaton.get_match_term_infos(segment_reader)?;
                        if term_infos.is_empty() {
                            // if term_infos is empty, that's fine -- we might have other terms
                            continue;
                        }
                        num_regex_terms += term_infos.len();
                        if num_regex_terms > max_expansions {
                            // we have more regex matches than our max_expansions -- stop matching now
                            continue;
                            // return Err(TantivyError::InvalidArgument(format!(
                            //     "Regex ProximityClause(s) exceeded max expansions: {num_regex_terms} > {max_expansions}",
                            // )));
                        }
                        let union = RegexPhraseWeight::get_union_from_term_infos(
                            &term_infos,
                            segment_reader,
                            &inverted_index,
                            self.weight_opt.is_some(),
                        )?;
                        postings.push(Box::new(union))
                    }
                }
            }
            Ok(ProxSide {
                postings,
                inner: None,
            })
        }
    }
}

impl Weight for ProximityWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        Ok(self
            .prox_scorer(reader, boost)?
            .map(|scorer| Box::new(scorer) as Box<dyn Scorer>)
            .unwrap_or_else(|| Box::new(EmptyScorer) as Box<dyn Scorer>))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let scorer_opt = self.prox_scorer(reader, 1.0)?;
        if scorer_opt.is_none() {
            return Err(does_not_match(doc));
        }
        let mut scorer = scorer_opt.unwrap();
        if scorer.doc() > doc || scorer.seek(doc) != doc {
            return Err(does_not_match(doc));
        }
        let fieldnorm_id = scorer.fieldnorm_id();
        let prox_count = scorer.prox_count();
        let mut explanation = Explanation::new("Proximity Scorer", scorer.score());
        if let Some(similarity_weight) = self.weight_opt.as_ref() {
            explanation.add_detail(similarity_weight.explain(fieldnorm_id, prox_count as u32));
        }
        Ok(explanation)
    }
}
