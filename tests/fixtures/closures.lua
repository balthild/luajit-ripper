local counter = 0
local shared = {}

local function bump(step)
	counter = counter + step
	return counter
end

local function make_accumulator(start)
	local total = start
	return function(value)
		total = total + value
		return total
	end
end

local function make_pair()
	local calls = 0
	local function inc()
		calls = calls + 1
		return calls
	end
	local function get()
		return calls
	end
	return inc, get
end

local function fact(n)
	if n <= 1 then
		return 1
	end
	return n * fact(n - 1)
end

local function outer()
	local function middle()
		local function inner()
			return bump(1) + shared.value
		end
		return inner()
	end
	return middle()
end

return {
	bump = bump,
	make_accumulator = make_accumulator,
	make_pair = make_pair,
	fact = fact,
	outer = outer,
}
