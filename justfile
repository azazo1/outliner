set dotenv-load := true

test-toc-direction:
    : "${OUTLINER_TOC_DIRECTION_TEST_PDF:?missing OUTLINER_TOC_DIRECTION_TEST_PDF}"
    : "${OUTLINER_TOC_DIRECTION_TEST_PAGES:?missing OUTLINER_TOC_DIRECTION_TEST_PAGES}"
    : "${OUTLINER_TOC_DIRECTION_TEST_EXPECTED:?missing OUTLINER_TOC_DIRECTION_TEST_EXPECTED}"
    cargo test toc_direction_live_test_on_specific_pdf_pages -- --ignored

extract-pages pdf pages:
    ./scripts/extract_pdf_pages.sh "{{ pdf }}" "{{ pages }}"
