-- Multi-line strings whose text can close the long bracket the writer emits for
-- them: one ends in `]`, one contains `]]`, and one contains both `]]` and
-- `]=]`, which pushes the delimiter up to two `=` signs.

local ending_in_bracket = "alpha\nbeta\ngamma\nends with a bracket ]"
local containing_double = "alpha\nbeta\ngamma\nholds ]] inside"
local containing_both = "alpha\nbeta\ngamma\nholds ]] and ]=] inside"
local short = "alpha\nbeta"

return {
	ending_in_bracket = ending_in_bracket,
	containing_double = containing_double,
	containing_both = containing_both,
	short = short,
}
