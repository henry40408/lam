Feature: A simple application to run Lua functions

  Scenario: Evaulate a Lua script
    Given a lua script
      | script        | result | input |
      |               |        |       |
      | return 1+1    | 2      |       |
      | return 'a'..1 | a1     |       |
      | return require('@lam')._VERSION   | 0.1.0 |     |
    When it is evaluated
    Then it should return result

  Scenario: Evaluate a Lua script w/ timeout
    Given a lua script
      | script            | result |
      | while true do end |        |
    When the timeout is set to 1 second
    And it is evaluated
    Then it should return result

  Scenario: Evaluate a Lua script w/ lam module
    Given a lua script
      | script                            | result | input |
      | return require('@lam'):read('*a') | lam    | lam   |
      | return require('@lam'):read(1)    | l      | lam   |
      | return require('@lam'):read(3)    | l      | l     |
      | return require('@lam'):read('*a') | 你好   | 你好  |
      | return require('@lam'):read_unicode(1) | l    | lam  |
      | return require('@lam'):read_unicode(2) | l    | l    |
      | return require('@lam'):read_unicode(1) | 你   | 你好 |
      | return require('@lam'):read_unicode(2) | 你   | 你   |
      | return require('@lam'):read_unicode(2) | 你好 | 你好 |
    When it is evaluated
    Then it should return result

  Scenario: Evaluate Lua examples
    Given a filename of lua script
      | filename        | input  | expected |
      | 01-hello.lua    |        |          |
      | 02-input.lua    | lua    |          |
      | 03-algebra.lua  | 2      | 4        |
      | 04-echo.lua     | a      | a        |
      | 05-store.lua    |        | 0        |
    When it is evaluated
    Then it should return result
