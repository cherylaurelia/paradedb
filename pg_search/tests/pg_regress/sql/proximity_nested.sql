-- A nested proximity clause matches as a span, from its first to its last token (#7174).
-- The outer clause measures distance from the span's ends, and a term inside it does not match.
CREATE EXTENSION IF NOT EXISTS pg_search;
DROP TABLE IF EXISTS prox_nested;
CREATE TABLE prox_nested (
    id serial8,
    text text
);
INSERT INTO prox_nested (text) VALUES
    ('a c b'),      -- 1: c inside a..b
    ('a b c'),      -- 2
    ('c a b'),      -- 3
    ('c b a x'),    -- 4
    ('b a c'),      -- 5
    ('a x b c'),    -- 6
    ('a b x c'),    -- 7: c two tokens after a..b
    ('b c a'),      -- 8: c inside b..a
    ('a b c a b'),  -- 9: two a..b matches
    ('x y a b'),    -- 10
    ('b a b c');    -- 11: a pairs with both b's, c is next to the second pair
CREATE INDEX idxprox_nested ON prox_nested USING paradedb (id, (text::pdb.simple));

-- the query from #7174: 2, 6, 9 and 11, not 1 (c inside a..b)
SELECT id, text FROM prox_nested WHERE text @@@ (pdb.prox_clause_in_order('a', 3, 'b') ##>0##> 'c') ORDER BY id;

-- the nested clause on the right: c right before a..b, so 3 and 9
SELECT id, text FROM prox_nested WHERE text @@@ ('c' ##>0##> pdb.prox_clause_in_order('a', 3, 'b')) ORDER BY id;

-- chains nest on the left
SELECT id, text FROM prox_nested WHERE text @@@ ('a' ##>1##> 'b' ##>0##> 'c') ORDER BY id;
SELECT id, text FROM prox_nested WHERE text @@@ ('a' ##1## 'b' ##0## 'c') ORDER BY id;
SELECT id, text FROM prox_nested WHERE text @@@ ('a' ##>0##> 'b' ##>1##> 'c' ##>0##> 'a') ORDER BY id;

-- a span on each side
SELECT id, text FROM prox_nested WHERE text @@@ pdb.proximity(pdb.prox_clause('a', 0, 'b'), 0, pdb.prox_clause('x', 0, 'y')) ORDER BY id;
SELECT id, text FROM prox_nested WHERE text @@@ pdb.proximity_in_order(pdb.prox_clause('x', 0, 'y'), 0, pdb.prox_clause('a', 0, 'b')) ORDER BY id;
SELECT id, text FROM prox_nested WHERE text @@@ pdb.proximity_in_order(pdb.prox_clause('a', 0, 'b'), 0, pdb.prox_clause('x', 0, 'y')) ORDER BY id;

-- every pair of the nested clause is a span: in 11, a..b ending at the second b is next to c
SELECT id, text FROM prox_nested WHERE text @@@ (pdb.prox_clause('a', 2, 'b') ##0## 'c') ORDER BY id;

-- an array inside the nested clause
SELECT id, text FROM prox_nested WHERE text @@@ (pdb.prox_clause(pdb.prox_array('a', 'x'), 0, 'b') ##0## 'c') ORDER BY id;

-- a term can share the last token of the nested match, as 'b' ##0## 'b' matches a single b
SELECT id, text FROM prox_nested WHERE text @@@ ('a' ##0## 'b' ##0## 'b') ORDER BY id;

-- nested matches are scored by how many pairs match: 9 has two a..b..c matches
SELECT id, round(pdb.score(id)::numeric, 4) AS score FROM prox_nested WHERE text @@@ ('a' ##1## 'b' ##0## 'c') ORDER BY id;

DROP TABLE prox_nested;
