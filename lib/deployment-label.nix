{
  source,
  release ? null,
}:

let
  versionTag =
    tag:
    builtins.stringLength tag <= 128
    &&
      builtins.match "v?[0-9]+[.][0-9]+[.][0-9]+(-[0-9A-Za-z-]+([.][0-9A-Za-z-]+)*)?([+][0-9A-Za-z-]+([.][0-9A-Za-z-]+)*)?" tag
      != null;
  validShort = value: value != null && builtins.match "[0-9a-f]{7}" value != null;
  short = full: abbreviated: if full != null then builtins.substring 0 7 full else abbreviated;
  dirty = short (source.dirtyRev or null) (source.dirtyShortRev or null);
  clean = short (source.rev or null) (source.shortRev or null);
  tags =
    if release != null && source ? rev && release.rev == source.rev then
      builtins.sort builtins.lessThan (builtins.filter versionTag release.tags)
    else
      [ ];
in
if validShort dirty then
  "${dirty}-dirty"
else if tags != [ ] then
  builtins.head tags
else if validShort clean then
  clean
else
  "unknown"
