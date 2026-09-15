//! Hints behavior tests. tdlib ships no dedicated `TEST(Hints, ...)` — the
//! expectations here are derived from `tdutils/td/utils/Hints.cpp` semantics
//! (search-word splitting, prefix + translit matching, rating order,
//! remove/rename re-indexing) and spot-checked against the C++ class.

use tdutils::hints::Hints;
use tdutils::translit::get_word_transliterations;
use tdutils::utf8::utf8_get_search_words;

#[test]
fn add_search_remove_and_rename() {
    let mut hints = Hints::default();
    hints.add(1, "Alice");
    hints.add(2, "Alina");
    hints.add(3, "Bob");
    assert_eq!(hints.size(), 3);
    assert!(hints.has_key(1));
    assert!(!hints.has_key(9));
    assert_eq!(hints.key_to_string(1), "Alice");
    assert_eq!(hints.key_to_string(9), "");

    // Prefix search over the folded name.
    let (total, keys) = hints.search("ali", 10, false);
    assert_eq!((total, keys), (2, vec![1, 2]));

    // Word-split multi-word queries intersect per word.
    hints.add(4, "Bob Marley");
    let (total, keys) = hints.search("bob mar", 10, false);
    assert_eq!((total, keys), (1, vec![4]));
    // Non-matching second word empties the result.
    let (total, keys) = hints.search("bob alice", 10, false);
    assert_eq!((total, keys), (0, vec![]));

    // `limit` truncates but the total count stands; negative limit counts only.
    let (total, keys) = hints.search("ali", 1, false);
    assert_eq!((total, keys.len()), (2, 1));
    let (total, keys) = hints.search("ali", -1, false);
    // limit < 0: the C++ early-returns the whole map size, not the match count.
    assert_eq!((total, keys), (4, vec![]));

    // Empty query: everything only with `return_all_for_empty_query`.
    let (total, keys) = hints.search("", 10, true);
    assert_eq!(total, 4);
    assert_eq!(keys.len(), 4);
    let (total, keys) = hints.search("", 10, false);
    assert_eq!((total, keys), (0, vec![]));
    let (total, keys) = hints.search_empty(2);
    assert_eq!(total, 4);
    assert_eq!(keys.len(), 2);

    // Removal takes the key out of every table.
    hints.remove(2);
    assert!(!hints.has_key(2));
    assert_eq!(hints.size(), 3);
    let (total, keys) = hints.search("ali", 10, false);
    assert_eq!((total, keys), (1, vec![1]));

    // Rename re-indexes (old words and translits dropped).
    hints.add(1, "Alicia");
    assert_eq!(hints.search("alice", 10, false), (0, vec![]));
    assert_eq!(hints.search("alicia", 10, false), (1, vec![1]));
    assert_eq!(hints.search("ali", 10, false), (1, vec![1]));

    // Re-adding the same name is a no-op.
    hints.add(1, "Alicia");
    assert_eq!(hints.size(), 3);
    assert_eq!(hints.search("alicia", 10, false), (1, vec![1]));
}

#[test]
fn rating_orders_results_and_breaks_ties_by_key() {
    let mut hints = Hints::default();
    for (key, name) in [(1i64, "aa"), (2, "ab"), (3, "ac")] {
        hints.add(key, name);
    }
    // Equal (default) ratings: key order.
    assert_eq!(hints.search("a", 10, false).1, vec![1, 2, 3]);
    hints.set_rating(3, -5);
    hints.set_rating(1, 10);
    assert_eq!(hints.search("a", 10, false).1, vec![3, 2, 1]);
    // Unrated keys behave as rating 0.
    hints.add(4, "ad");
    assert_eq!(hints.search("a", 10, false).1, vec![3, 2, 4, 1]);
}

#[test]
fn translit_matches_in_both_directions() {
    let mut hints = Hints::default();
    hints.add(1, "privet");
    // Latin name found by a Cyrillic query (en→ru).
    assert_eq!(hints.search("привет", 10, false), (1, vec![1]));
    // Partial Cyrillic query matches through the translit table.
    assert_eq!(hints.search("прив", 10, false), (1, vec![1]));

    hints.add(2, "привет");
    // Cyrillic name found by the Latin query (ru→en), plus the direct hit.
    assert_eq!(hints.search("privet", 10, false), (2, vec![1, 2]));
    assert_eq!(hints.search("прив", 10, false), (2, vec![1, 2]));

    // Complex rules: "shch" ↔ "щ".
    hints.add(3, "shchuka");
    assert_eq!(hints.search("щука", 10, false), (1, vec![3]));
    hints.add(4, "щука");
    assert_eq!(hints.search("shchuka", 10, false), (2, vec![3, 4]));
}

#[test]
fn search_words_split_and_fold() {
    // Punctuation separates, digits stay, case folds, combining marks drop.
    assert_eq!(
        utf8_get_search_words("Bob_Marley Jr."),
        vec!["bob".to_owned(), "marley".to_owned(), "jr".to_owned()]
    );
    assert_eq!(
        utf8_get_search_words("user123"),
        vec!["user123".to_owned()]
    );
    assert_eq!(utf8_get_search_words("Владимир Путин"), vec!["владимир".to_owned(), "путин".to_owned()]);
    // Precomposed é folds to e; the combining-acute form drops the mark —
    // both spellings produce the same word.
    assert_eq!(utf8_get_search_words("José"), vec!["jose".to_owned()]);
    assert_eq!(
        utf8_get_search_words("Jose\u{301}"),
        vec!["jose".to_owned()]
    );
    assert!(utf8_get_search_words(" ..!! ").is_empty());
}

#[test]
fn fix_words_drops_prefixes_of_later_words() {
    let words = vec![
        "bob".to_owned(),
        "bob".to_owned(),
        "bobmarley".to_owned(),
        "alice".to_owned(),
    ];
    assert_eq!(
        Hints::fix_words(words),
        vec!["alice".to_owned(), "bobmarley".to_owned()]
    );
    assert!(Hints::fix_words(vec!["".to_owned()]).is_empty());
    assert!(Hints::fix_words(Vec::new()).is_empty());
}

#[test]
fn transliteration_table_shapes() {
    // Simple both-ways candidates plus the word itself from the opposite
    // direction's pass-through, deduplicated and sorted.
    let variants = get_word_transliterations("privet", false);
    assert!(variants.contains(&"привет".to_owned()));
    let variants = get_word_transliterations("привет", false);
    assert!(variants.contains(&"privet".to_owned()));
    // Partial mode offers the complex-rule completions ("sh" → "ш"/"щ").
    let variants = get_word_transliterations("sh", true);
    assert!(variants.contains(&"ш".to_owned()));
    assert!(variants.contains(&"щ".to_owned()));
}
