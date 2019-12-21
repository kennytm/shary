#!/bin/sh
set -ex
html-minifier-terser \
    --collapse-boolean-attributes \
    --collapse-inline-tag-whitespace \
    --collapse-whitespace \
    --decode-entities \
    --remove-attribute-quotes \
    --remove-comments \
    --remove-optional-tags \
    --remove-redundant-attributes \
    --sort-attributes \
    --sort-class-name \
    --minify-css true \
    --minify-js true \
    -o src/web/index.min.html \
    src/web/index.html
gzip -f -9 src/web/index.min.html
