require 'minitest/autorun'

require 'pf2'

class SessionTest < Minitest::Test
  def test_default_options
    config = Pf2::Session.new.configuration
    assert_equal(:signal, config[:scheduler])
    assert_equal(49, config[:interval_ms])
    assert_equal(:cpu, config[:time_mode])
  end

  def test_scheduler_option
    config = Pf2::Session.new(scheduler: :timer_thread).configuration
    assert_equal(:timer_thread, config[:scheduler])
  end

  def test_interval_ms_option
    config = Pf2::Session.new(interval_ms: 1).configuration
    assert_equal(1, config[:interval_ms])
  end

  def test_time_mode_option
    config = Pf2::Session.new(time_mode: :wall).configuration
    assert_equal(:wall, config[:time_mode])
  end
end
