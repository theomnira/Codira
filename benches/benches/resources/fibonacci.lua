function fibonacci(n)
    local a, b, i = 0, 1, 1
    repeat
        if i > n then
            return a
        end
        local sum = a + b
        a, b = b, sum
        i = i + 1
    until false
end

function main(n)
    return fibonacci(n)
end
