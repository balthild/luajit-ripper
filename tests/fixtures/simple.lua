local function greet(name)
	return "Hello, " .. name .. "!"
end

local function main()
	local names = { "world", "lua" }
	local out = {}
	for i = 1, #names do
		out[i] = greet(names[i])
	end
	return table.concat(out, ", ")
end

local value = main()
print(value)
return value
