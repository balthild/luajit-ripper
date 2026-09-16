-- A conditional with an empty body, followed by a short circuit expression.
-- LuaJIT compiles the empty body into a branch whose two targets are the same
-- address, which makes the decompiler insert a placeholder block. Getting the
-- body boundary of that placeholder wrong used to make the `ISTC` that starts
-- the `or` expression look like a statement.
local function pick(kind, fallback)
	local name = nil

	if kind ~= 1 and kind == 2 then
	end

	return name or fallback or "default"
end

local function pick_else(kind, fallback)
	local name = nil

	if kind == 1 then
		name = "one"
	else
	end

	return name or fallback
end

return { pick = pick, pick_else = pick_else }
