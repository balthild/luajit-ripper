local template = { 1, 2, 3, name = "template", nested = { a = 1 } }

local function build()
	local t = {}
	t[1] = "one"
	t[2] = "two"
	t.name = "built"
	t["quoted key"] = true
	t[3] = nil
	return t
end

local function copy()
	return { 1, 2, 3, name = "template", nested = { a = 1 } }
end

local function access(t, key)
	local a = t.field
	local b = t["other"]
	local c = t[key]
	t.field = a + 1
	t[key] = b
	return a, b, c
end

local mixed = { template[1], other = build(), [1 + 1] = "computed" }

return { build = build, copy = copy, access = access, mixed = mixed }
