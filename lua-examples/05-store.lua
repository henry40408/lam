local m = require("@lam")
return m:get_set("a", function(v)
	return v + 1
end, 0)
