local function pick(...)
	local count = select("#", ...)
	if count == 0 then
		return nil
	end
	return ...
end

local function multi()
	return 1, 2, 3
end

local function spread(...)
	local a, b, c = ...
	local t = { ... }
	local u = { multi() }
	return a, b, c, t, u
end

local function tail(...)
	return pick(...)
end

local object = { value = 10 }

function object:add(other)
	self.value = self.value + other.value
	return self
end

function object.static(a, b)
	return a + b
end

local function use()
	local result = object:add({ value = 5 }).value
	local sum = object.static(1, 2)
	local first, second = multi()
	return result, sum, first, second, spread(1, 2, 3), tail(1, 2)
end

print(use())
